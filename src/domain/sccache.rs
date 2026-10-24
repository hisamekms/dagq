//! The sccache server of a `[run.env]` whose `RUSTC_WRAPPER` is sccache
//! (ADR-t1215-1): the supervisor, outside any sandbox, starts and keeps
//! it; no other process the runtime gives `[run.env]` may (ADR-t2086-1),
//! sandboxed or not: each is refused the server's start, one whose server
//! could not be confirmed just before it starts runs without
//! `RUSTC_WRAPPER`, and one whose server was confirmed compiles through
//! the guard, which runs the compiler itself should the server stop after
//! the look ([`GuardLook`]).
//!
//! A worker's or a resume's turn in its container has a server of its own
//! inside the container when `[run.env]` names sccache (ADR-t2114-4,
//! ADR-t2257-1 decision 5): the supervisor starts it before the agent
//! runs, its cache in the queue's worker cache ([`ContainerServer`]), and
//! the container's launcher gives the agent the guard behind the gate the
//! start left ([`ContainerGate`], [`gate_step`]).

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
/// The server of a run's container the control side started, or failed to
/// (ADR-t2114-4 decision 1): events of the run, never the host server's.
pub const SCCACHE_CONTAINER_SERVER_STARTED: &str =
    crate::domain::event_kind::EventKind::SccacheContainerServerStarted.as_str();
pub const SCCACHE_CONTAINER_SERVER_START_FAILED: &str =
    crate::domain::event_kind::EventKind::SccacheContainerServerStartFailed.as_str();

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
    /// Before the wrapper of a headless run's session lost while it waits
    /// is started again.
    BeforeReopen,
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
            Self::BeforeReopen => "before_reopen",
            Self::BeforeReview => "before_review",
            Self::BeforeIntegrate => "before_integrate",
            Self::BeforeRecheck => "before_recheck",
            Self::BeforeE2e => "before_e2e",
        }
    }
}

/// What the supervisor's look does once it knows whether the server
/// listens and whether the wait after its last failed start (or restart)
/// is still on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LookStep {
    /// None listens and the last start failed within the wait: nothing is
    /// started, and the server is not confirmed.
    WaitToStart,
    /// One listens, but its last restart failed within the wait: it is not
    /// confirmed.
    WaitToRestart,
    /// One listens: its health is observed.
    Observe,
    /// None listens: the supervisor starts it.
    Start,
}

impl LookStep {
    pub const fn of(listening: bool, retry_waits: bool) -> Self {
        match (listening, retry_waits) {
            (false, true) => Self::WaitToStart,
            (false, false) => Self::Start,
            (true, true) => Self::WaitToRestart,
            (true, false) => Self::Observe,
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

/// The variable that names sccache's local cache directory: the server of
/// a run's container keeps its cache there.
pub const CACHE_DIR_VAR: &str = "SCCACHE_DIR";

/// The variable a turn's container is given when its server is to be
/// started (ADR-t2114-4 decisions 1 and 2): the path of the turn's gate
/// file in the run directory ([`container_gate`]), which the container's
/// launcher waits for before it runs the agent and obeys ([`gate_step`]).
pub const CONTAINER_GATE_VAR: &str = "DAGQ_SCCACHE_GATE";
/// The variable a turn's container is given beside [`CONTAINER_GATE_VAR`]:
/// the turn's own token, which no other turn of the run has and which
/// names the turn's gate and the request its launcher leaves
/// ([`container_gate`], [`container_request`]), so that an answer to one
/// turn acts on no later turn's.
pub const CONTAINER_TURN_VAR: &str = "DAGQ_SCCACHE_TURN";
/// The start of the name of a turn's gate file in the run directory, the
/// rest being the turn's token: written once per turn, whole, and only
/// where none is (a file of its own linked to the name, which fails where
/// one is), by the supervisor with its verdict or by the launcher that
/// waited for none in time; the first one there is the turn's, and both
/// obey it ([`ContainerGate`]).
pub const CONTAINER_GATE_PREFIX: &str = "sccache-gate.";
/// The start of the name of a request in the run directory, which the
/// launcher leaves once it waits for the gate: the supervisor's sign that
/// the turn's container runs and waits for its server. The rest of the
/// name is the turn's token.
pub const CONTAINER_REQUEST_PREFIX: &str = "sccache-request.";

/// `prefix` and the token `turn`; `None` for a token that is empty or has
/// anything but ASCII letters and digits.
fn turn_file(prefix: &str, turn: &str) -> Option<String> {
    (!turn.is_empty() && turn.bytes().all(|byte| byte.is_ascii_alphanumeric()))
        .then(|| format!("{prefix}{turn}"))
}

/// The name of the gate of the turn whose token is `turn`
/// ([`CONTAINER_GATE_PREFIX`]); `None` for a token that names none.
pub fn container_gate(turn: &str) -> Option<String> {
    turn_file(CONTAINER_GATE_PREFIX, turn)
}

/// The name of the request of the turn whose token is `turn`
/// ([`CONTAINER_REQUEST_PREFIX`]); `None` for a token that names none.
pub fn container_request(turn: &str) -> Option<String> {
    turn_file(CONTAINER_REQUEST_PREFIX, turn)
}

/// The token of the turn the request `name` is of ([`container_request`]);
/// `None` for a name that is no request's.
pub fn request_turn(name: &str) -> Option<&str> {
    name.strip_prefix(CONTAINER_REQUEST_PREFIX)
        .filter(|turn| container_request(turn).is_some())
}

/// Whether `name` is the name of a turn's request ([`container_request`]).
pub fn is_container_request(name: &str) -> bool {
    request_turn(name).is_some()
}

/// Whether `name` is the name of a turn's gate ([`container_gate`]).
pub fn is_container_gate(name: &str) -> bool {
    name.strip_prefix(CONTAINER_GATE_PREFIX)
        .is_some_and(|turn| container_gate(turn).is_some())
}

/// The guard of a turn's container in the run directory `run_dir`: a link
/// to the container's `dagq` in the agent's home, which the supervisor
/// makes in the container (`dagq relay guard`) before it opens the gate.
pub fn container_guard(run_dir: &Path) -> std::path::PathBuf {
    run_dir
        .join(super::run_container::HOME_DIR)
        .join(GUARD_NAME)
}

/// What the gate of a turn's container says, the one text both its
/// writers and its launcher read the same way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContainerGate {
    /// The supervisor started the server and made the guard: the agent
    /// compiles through the guard.
    Started,
    /// The supervisor could not start the server or make the guard (why):
    /// the agent runs without `RUSTC_WRAPPER`.
    Failed(String),
    /// The launcher found no gate within its wait and wrote this one
    /// (why): the agent runs without `RUSTC_WRAPPER`.
    TimedOut(String),
}

impl ContainerGate {
    /// The file's text: `started`, `failed: <why>` or `timed out: <why>`.
    pub fn text(&self) -> String {
        match self {
            Self::Started => "started".to_owned(),
            Self::Failed(why) => format!("failed: {why}"),
            Self::TimedOut(why) => format!("timed out: {why}"),
        }
    }

    /// The gate a file's text says; `None` for an empty one (no writer
    /// leaves one, so it is not settled). Any other text is
    /// [`Self::Failed`], so that no text a worker left opens it wider.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        if text.is_empty() {
            return None;
        }
        Some(if text == "started" {
            Self::Started
        } else if let Some(why) = text.strip_prefix("failed: ") {
            Self::Failed(why.to_owned())
        } else if let Some(why) = text.strip_prefix("timed out: ") {
            Self::TimedOut(why.to_owned())
        } else {
            Self::Failed(format!("the gate {text:?} is not one"))
        })
    }
}

/// What the launcher's wait for the gate does next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateStep {
    /// Look again.
    Wait,
    /// The gate is there: give the agent what it says.
    Obey(ContainerGate),
    /// None came within the wait: write [`ContainerGate::TimedOut`] where
    /// none is, then obey whichever gate is there.
    Claim,
    /// A gate there that says nothing in time: run no agent (why), since
    /// no environment would agree with what is recorded.
    Abort(String),
}

/// The launcher's next step after `waited` of its wait `wait`, the gate
/// file's text being `read` (`None` while there is none; an empty text
/// for one that is there but cannot be read). A gate that says nothing is
/// waited for up to twice the wait.
pub fn gate_step(
    read: Option<&str>,
    waited: std::time::Duration,
    wait: std::time::Duration,
) -> GateStep {
    match read.map(ContainerGate::parse) {
        Some(Some(gate)) => GateStep::Obey(gate),
        Some(None) if waited >= wait.saturating_mul(2) => {
            GateStep::Abort(format!("the sccache gate stayed empty for {waited:?}"))
        }
        None if waited >= wait => GateStep::Claim,
        _ => GateStep::Wait,
    }
}

/// The variables the launcher of a turn's container gives the agent about
/// sccache once it obeys `gate` (ADR-t2114-4 decision 2): always the
/// [`REFUSED_ERROR_LOG`], so that no sccache it runs starts a server;
/// behind [`ContainerGate::Started`] alone, the guard `guard` the
/// supervisor made as `RUSTC_WRAPPER` and `program` as the sccache it
/// compiles through. The guard looks at the server's port on the
/// container's own loopback.
pub fn container_agent_env(
    program: &str,
    gate: &ContainerGate,
    guard: &str,
) -> Vec<(String, String)> {
    let target = SccacheTarget {
        program: program.to_owned(),
        port: DEFAULT_PORT,
    };
    guarded_env(&target, (*gate == ContainerGate::Started).then_some(guard))
}

/// What the supervisor does at a turn container's request, by the gate it
/// finds there (`None` for none), once no record of the container is
/// there yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestStep {
    /// No gate yet: start the server, make the guard and write the gate.
    Start,
    /// A gate is the turn's already and nothing records it yet (the
    /// launcher's timed out one, or the supervisor's own that a process
    /// before this one wrote and did not record): record what it says.
    Record(ContainerGate),
    /// A gate being written: look again on the next pass.
    Wait,
}

impl RequestStep {
    pub fn of(gate: Option<&str>) -> Self {
        match gate.map(ContainerGate::parse) {
            None => Self::Start,
            Some(Some(gate)) => Self::Record(gate),
            Some(None) => Self::Wait,
        }
    }
}

/// The supervisor's verdict on a turn container's server: started only
/// when the server started and the guard was made.
pub fn container_verdict(
    started: &Result<(), String>,
    guard: &Result<(), String>,
) -> ContainerGate {
    match (started, guard) {
        (Err(why), _) => ContainerGate::Failed(format!(
            "the container's sccache server was not started: {why}"
        )),
        (Ok(()), Err(why)) => {
            ContainerGate::Failed(format!("the guard was not made in the container: {why}"))
        }
        (Ok(()), Ok(())) => ContainerGate::Started,
    }
}

/// How the supervisor's write of its gate went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateClaim {
    /// Its gate is the turn's.
    Won,
    /// A gate was there first, with this text.
    Lost(String),
    /// None could be written (why): the launcher writes its own once its
    /// wait ends, or runs no agent.
    Failed(String),
}

/// Why the agent of a turn's container runs without `RUSTC_WRAPPER` once
/// the supervisor wrote `verdict` as `claim` says, to record as
/// `sccache_wrapper_removed`; `None` when it compiles through the guard or
/// another writer records it. It agrees with the gate the launcher obeys:
/// the supervisor's when it won, the launcher's timed out one when that
/// was there first, and the launcher's timed out one to come (or no agent)
/// when none could be written.
pub fn removed_after(verdict: &ContainerGate, claim: &GateClaim) -> Option<String> {
    match claim {
        GateClaim::Won => match verdict {
            ContainerGate::Started => None,
            ContainerGate::Failed(why) | ContainerGate::TimedOut(why) => Some(why.clone()),
        },
        GateClaim::Lost(text) => match ContainerGate::parse(text) {
            Some(ContainerGate::TimedOut(why)) => Some(why),
            _ => None,
        },
        GateClaim::Failed(why) => Some(format!(
            "the sccache gate could not be written: {why}; the agent runs without {WRAPPER_VAR}, or none runs"
        )),
    }
}

/// What the supervisor does about sccache for a turn's container of a
/// worker or a resume (ADR-t2114-4 decisions 1 to 3, ADR-t2257-1
/// decision 5), from the run's `[run.env]` and the directories the queue's
/// worker cache is mounted at.
///
/// The container never reaches the host's server or its cache. Only the
/// supervisor starts the container's server (ADR-t2114-4 decision 1):
/// the session wrapper gives the container a token of the turn's own and
/// the path of the gate it names, and clears the earlier turns' gates and
/// requests; the container's launcher leaves the request the token names
/// once it waits; the supervisor, at a request of a run it leases, starts
/// the server and makes the guard in the container by `podman exec`,
/// writes its verdict as the gate of that container's turn, records the
/// start or its failure on the run (`sccache_container_server_*`), and
/// takes away only the requests it listed before it looked at the
/// container. What an answer writes and takes is its turn's, never a later
/// turn's, whenever that turn's container replaces the one answered. The
/// gate is written once ([`CONTAINER_GATE_PREFIX`]): the launcher that
/// waited for none in time writes its own, and the event follows whichever
/// gate is the turn's ([`removed_after`]). The agent is always refused the
/// server's start and compiles through the guard only behind a started
/// gate ([`container_agent_env`]); otherwise it runs without
/// `RUSTC_WRAPPER`, the image's too (the container is started without it),
/// recorded as `sccache_wrapper_removed`. A `[run.env]`
/// naming no sccache is given as it is, and a verification's container
/// never has a server (ADR-t2114-4 decision 4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContainerServer {
    /// `[run.env]` names no sccache: no server is started and the
    /// environment is left as it is.
    NotNamed,
    /// Start the server in the container with `env` (its `[run.env]`
    /// without `RUSTC_WRAPPER` and [`ERROR_LOG_VAR`], with the idle timeout
    /// that never stops it) before the agent runs.
    Start {
        target: SccacheTarget,
        env: Vec<(String, String)>,
    },
    /// No server is started and the agent runs without `RUSTC_WRAPPER`,
    /// recorded with `why`: the cache directory the server would use is
    /// not in the queue's worker cache, so it would be the run's own (no
    /// [`CACHE_DIR_VAR`] puts it in the agent's home) or a directory no
    /// other run's server shares.
    Refused { target: SccacheTarget, why: String },
}

impl ContainerServer {
    /// The plan of a turn's container whose `[run.env]` is `run_env` and
    /// whose queue's worker cache is mounted at `worker_cache`. The
    /// server's cache directory ([`CACHE_DIR_VAR`]) must be one of those
    /// directories or under one: the queue's shared worker cache, never
    /// copied for a run from a seed nor gathered into one server.
    pub fn of(run_env: &[(String, String)], worker_cache: &[String]) -> Self {
        let Some(target) = SccacheTarget::of_pairs(run_env) else {
            return Self::NotNamed;
        };
        let dir = run_env
            .iter()
            .rev()
            .find(|(key, _)| key == CACHE_DIR_VAR)
            .map(|(_, value)| value.as_str());
        let Some(dir) = dir.filter(|dir| !dir.is_empty()) else {
            return Self::Refused {
                target,
                why: format!(
                    "[run.env] sets no {CACHE_DIR_VAR}: the container's sccache server would keep its cache in the run's home, not the queue's worker cache"
                ),
            };
        };
        if !worker_cache
            .iter()
            .any(|mount| within(Path::new(dir), Path::new(mount)))
        {
            return Self::Refused {
                target,
                why: format!(
                    "{CACHE_DIR_VAR} {dir} is not in a directory of [container.worker_cache]: the container's sccache server keeps its cache only in the queue's worker cache"
                ),
            };
        }
        let mut env: Vec<(String, String)> = run_env
            .iter()
            .filter(|(key, _)| {
                ![WRAPPER_VAR, ERROR_LOG_VAR, IDLE_TIMEOUT_VAR].contains(&key.as_str())
            })
            .cloned()
            .collect();
        env.push((IDLE_TIMEOUT_VAR.to_owned(), IDLE_TIMEOUT.to_owned()));
        Self::Start { target, env }
    }
}

/// Whether the absolute path `path` is `dir` or under it, by whole
/// components: `/cache/sccache` is under `/cache`, `/cachex` is not, and a
/// `..` component is never under anything.
fn within(path: &Path, dir: &Path) -> bool {
    use std::path::Component;
    path.is_absolute()
        && dir.is_absolute()
        && !path.components().any(|part| part == Component::ParentDir)
        && path.starts_with(dir)
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

    /// The look before a lost session's wrapper starts again: a missing
    /// server is started, one that listens is observed, a wait after a
    /// failed start holds both; a `[run.env]` naming no sccache is no
    /// target, so nothing is looked at and the session opens as it would.
    #[test]
    fn a_reopen_starts_a_missing_server_and_leaves_an_unconfigured_one_alone() {
        assert_eq!(CheckReason::BeforeReopen.as_str(), "before_reopen");
        assert_eq!(LookStep::of(false, false), LookStep::Start);
        assert_eq!(LookStep::of(true, false), LookStep::Observe);
        assert_eq!(LookStep::of(false, true), LookStep::WaitToStart);
        assert_eq!(LookStep::of(true, true), LookStep::WaitToRestart);
        assert_eq!(target(&[("PATH", "/usr/bin")]), None);
        assert_eq!(target(&[(WRAPPER_VAR, "/usr/bin/ccache")]), None);
    }

    #[test]
    fn the_reasons_have_their_names() {
        let names: Vec<&str> = [
            CheckReason::Startup,
            CheckReason::Missing,
            CheckReason::BeforeWorker,
            CheckReason::BeforeResume,
            CheckReason::BeforeReopen,
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
                "before_reopen",
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

    fn pairs(env: &[(&str, &str)]) -> Vec<(String, String)> {
        env.iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    /// The container's server starts only for a `[run.env]` naming sccache
    /// whose cache directory is in the queue's worker cache, with the
    /// environment that lets it start and never stops it on idle.
    #[test]
    fn the_container_s_server_starts_only_with_its_cache_in_the_queue_s_worker_cache() {
        let cache = ["/cache/registry".to_owned(), "/cache/sccache".to_owned()];
        assert_eq!(
            ContainerServer::of(&pairs(&[("CARGO_BUILD_JOBS", "2")]), &cache),
            ContainerServer::NotNamed
        );
        assert_eq!(
            ContainerServer::of(&pairs(&[("RUSTC_WRAPPER", "cachepot")]), &cache),
            ContainerServer::NotNamed
        );
        let named = pairs(&[
            ("RUSTC_WRAPPER", "sccache"),
            ("SCCACHE_SERVER_PORT", "4300"),
            ("SCCACHE_ERROR_LOG", "/tmp/log"),
            ("SCCACHE_IDLE_TIMEOUT", "600"),
            ("SCCACHE_DIR", "/cache/sccache/dagq"),
        ]);
        let ContainerServer::Start { target, env } = ContainerServer::of(&named, &cache) else {
            panic!("not started")
        };
        assert_eq!(target.port, 4300);
        assert_eq!(
            env,
            pairs(&[
                ("SCCACHE_SERVER_PORT", "4300"),
                ("SCCACHE_DIR", "/cache/sccache/dagq"),
                ("SCCACHE_IDLE_TIMEOUT", "0"),
            ])
        );
        for dir in [
            None,
            Some(""),
            Some("/cachex"),
            Some("/cache/sccache/../../etc"),
            Some("cache/sccache"),
        ] {
            let mut env = pairs(&[("RUSTC_WRAPPER", "/usr/bin/sccache")]);
            if let Some(dir) = dir {
                env.push(("SCCACHE_DIR".to_owned(), dir.to_owned()));
            }
            let plan = ContainerServer::of(&env, &cache);
            assert!(
                matches!(&plan, ContainerServer::Refused { target, why }
                    if target.port == DEFAULT_PORT && why.contains("SCCACHE_DIR")),
                "{dir:?}: {plan:?}"
            );
        }
        // No worker cache mounted: no shared directory to keep it in.
        assert!(matches!(
            ContainerServer::of(&named, &[]),
            ContainerServer::Refused { .. }
        ));
    }

    /// A turn's request and gate are named by its token alone: one that
    /// could name another entry (a path, a dot) names neither.
    #[test]
    fn a_turn_s_request_and_gate_are_named_by_its_token_alone() {
        assert_eq!(
            container_request("0123abcd").as_deref(),
            Some("sccache-request.0123abcd")
        );
        for turn in ["", "../gate", "a.b", "a/b", "a b"] {
            assert_eq!(container_request(turn), None, "{turn:?}");
        }
        assert_eq!(
            container_gate("0123abcd").as_deref(),
            Some("sccache-gate.0123abcd")
        );
        assert_eq!(container_gate("a/b"), None);
        assert!(is_container_request("sccache-request.0123abcd"));
        assert_eq!(request_turn("sccache-request.0123abcd"), Some("0123abcd"));
        assert!(is_container_gate("sccache-gate.0123abcd"));
        for name in [
            "sccache-request",
            "sccache-request.",
            "sccache-request.a.tmp",
            ".sccache-request.a",
            "sccache-gate.a",
        ] {
            assert!(!is_container_request(name), "{name}");
        }
        for name in [
            "sccache-gate",
            "sccache-gate.",
            ".sccache-gate.a.b.tmp",
            "sccache-request.a",
        ] {
            assert!(!is_container_gate(name), "{name}");
        }
    }

    /// The gate's text reads back as written, an empty one is being
    /// written, and any other text keeps the gate closed.
    #[test]
    fn the_gate_reads_back_and_any_other_text_keeps_it_closed() {
        for gate in [
            ContainerGate::Started,
            ContainerGate::Failed("no server".to_owned()),
            ContainerGate::TimedOut("no gate in 180s".to_owned()),
        ] {
            assert_eq!(ContainerGate::parse(&gate.text()), Some(gate.clone()));
        }
        assert_eq!(ContainerGate::parse(" \n"), None);
        assert_eq!(
            ContainerGate::parse("started\n"),
            Some(ContainerGate::Started)
        );
        for text in ["Started", "started now", "open"] {
            assert!(
                matches!(ContainerGate::parse(text), Some(ContainerGate::Failed(why)) if why.contains(text)),
                "{text}"
            );
        }
        assert_eq!(
            container_guard(Path::new("/q/runs/r1")),
            Path::new("/q/runs/r1/container-home/dagq-rustc-wrapper")
        );
    }

    /// Acceptance (2) of task 2181: the launcher waits for the gate, writes
    /// its own once the wait ends, waits for a gate being written up to
    /// twice the wait, and gives the agent the refused log always and the
    /// guard only behind a started gate. The times are values.
    #[test]
    fn the_launcher_waits_for_the_gate_and_guards_the_agent_only_behind_a_started_one() {
        use std::time::Duration;
        let wait = Duration::from_secs(180);
        let at = Duration::from_secs;
        assert_eq!(gate_step(None, at(179), wait), GateStep::Wait);
        assert_eq!(gate_step(None, wait, wait), GateStep::Claim);
        assert_eq!(
            gate_step(Some("started"), at(999), wait),
            GateStep::Obey(ContainerGate::Started)
        );
        assert_eq!(gate_step(Some(""), at(359), wait), GateStep::Wait);
        assert!(matches!(
            gate_step(Some(""), at(360), wait),
            GateStep::Abort(_)
        ));

        let refused = ("SCCACHE_ERROR_LOG".to_owned(), REFUSED_ERROR_LOG.to_owned());
        let guard = "/q/runs/r1/container-home/dagq-rustc-wrapper";
        assert_eq!(
            container_agent_env("sccache", &ContainerGate::Started, guard),
            [
                refused.clone(),
                ("RUSTC_WRAPPER".to_owned(), guard.to_owned()),
                ("DAGQ_SCCACHE_PROGRAM".to_owned(), "sccache".to_owned()),
            ]
        );
        for gate in [
            ContainerGate::Failed("x".to_owned()),
            ContainerGate::TimedOut("y".to_owned()),
        ] {
            assert_eq!(
                container_agent_env("sccache", &gate, guard),
                std::slice::from_ref(&refused),
                "{gate:?}"
            );
        }
    }

    /// Acceptance (3) of task 2181: what the supervisor records follows the
    /// gate the launcher obeys. Its own verdict when it won; the
    /// launcher's timed out gate when that was there first, late as the
    /// supervisor may be; the launcher's gate to come (or no agent) when
    /// its own could not be written.
    #[test]
    fn the_supervisor_records_what_the_gate_the_launcher_obeys_says() {
        let ok = Ok(());
        let no = |why: &str| Err(why.to_owned());
        assert_eq!(container_verdict(&ok, &ok), ContainerGate::Started);
        assert!(
            matches!(container_verdict(&no("exit 2"), &ok), ContainerGate::Failed(why) if why.contains("not started: exit 2"))
        );
        assert!(
            matches!(container_verdict(&ok, &no("a directory")), ContainerGate::Failed(why) if why.contains("guard was not made in the container: a directory"))
        );

        let started = ContainerGate::Started;
        let failed = ContainerGate::Failed("the guard was not made".to_owned());
        let timed_out = ContainerGate::TimedOut("no gate within 180s".to_owned());
        assert_eq!(removed_after(&started, &GateClaim::Won), None);
        assert_eq!(
            removed_after(&failed, &GateClaim::Won).as_deref(),
            Some("the guard was not made")
        );
        // The launcher's gate came first: its agent has no wrapper,
        // whatever the supervisor started.
        assert_eq!(
            removed_after(&started, &GateClaim::Lost(timed_out.text())).as_deref(),
            Some("no gate within 180s")
        );
        for there in [started.text(), failed.text(), String::new()] {
            assert_eq!(
                removed_after(&started, &GateClaim::Lost(there.clone())),
                None,
                "{there}"
            );
        }
        assert!(
            removed_after(&started, &GateClaim::Failed("EACCES".to_owned()))
                .is_some_and(|why| why.contains("EACCES"))
        );

        // At a request: no gate is a start; a gate no record has yet
        // (the launcher's, or one a process before this one wrote) is
        // recorded as it says; one that says nothing is looked at again.
        assert_eq!(RequestStep::of(None), RequestStep::Start);
        for gate in [started, failed, timed_out] {
            assert_eq!(
                RequestStep::of(Some(&gate.text())),
                RequestStep::Record(gate.clone())
            );
        }
        assert_eq!(RequestStep::of(Some("")), RequestStep::Wait);
    }
}
