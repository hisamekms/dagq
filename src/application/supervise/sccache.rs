//! The sccache server of a `[run.env]` whose `RUSTC_WRAPPER` is sccache
//! (ADR-t1215-1). sccache's client starts the server it does not find, and
//! a server started in a sandbox keeps the sandbox: every build through it
//! fails then. So the supervisor, outside any sandbox, looks at the server
//! at its start, every [`LOOK_INTERVAL`] after, and just before a Codex
//! worker's workspace, a Codex resume or a Codex review opens; it starts
//! a missing one with `SCCACHE_IDLE_TIMEOUT=0` and records
//! `sccache_server_started` (or `sccache_server_start_failed`). The look
//! is a connect to the server's loopback port. Only confirmed existing
//! servers are queried for process identity and stats; foreign servers
//! are recorded, and failure-only deltas or known confinement cause a
//! stop and restart outside the sandbox. A Codex review is given the
//! refusal of the server's start and, when its server was confirmed,
//! compiles through the guard (ADR-t2008-1); one whose server could not be
//! confirmed runs without `RUSTC_WRAPPER`, recorded as
//! `sccache_wrapper_removed`. A Codex worker's turns are looked at by its
//! wrapper the same way ([`crate::application::headless_session`]).

use super::*;
use crate::application::SccacheServer;
use crate::domain::sccache::{
    CheckReason, IDLE_TIMEOUT, IDLE_TIMEOUT_VAR, SccacheTarget, ServerCheck, WRAPPER_VAR,
    sandbox_env,
};

/// How often a pass looks at the server.
pub(super) const LOOK_INTERVAL: Duration = Duration::from_secs(10);
/// How long after a failed start the next one is tried.
pub(super) const RETRY_AFTER: Duration = Duration::from_secs(60);

/// What the supervisor keeps the host's sccache server with.
#[derive(Clone)]
pub struct SccachePort(pub Arc<dyn SccacheServer + Send + Sync>);

/// When the server was last looked at and a start last failed.
#[derive(Default)]
pub(super) struct SccacheWatch {
    last_look: Option<Instant>,
    failed_at: Option<Instant>,
    failures: crate::domain::sccache::FailureWatch,
}

impl Supervisor<'_> {
    /// Look at the server every [`LOOK_INTERVAL`], draining or not: the
    /// first look is the start's.
    pub(super) fn sccache_pass(&mut self) {
        if self.sccache_port.is_none() {
            return;
        }
        let reason = match self.sccache.last_look {
            None => CheckReason::Startup,
            Some(at) if self.generators.clock.monotonic().duration_since(at) < LOOK_INTERVAL => {
                return;
            }
            Some(_) => CheckReason::Missing,
        };
        self.ensure_sccache(reason);
    }

    /// Look at the server and start it when none listens. Not configured
    /// without a port or a `[run.env]` whose `RUSTC_WRAPPER` is sccache.
    pub(super) fn ensure_sccache(&mut self, reason: CheckReason) -> ServerCheck {
        let Some(SccachePort(server)) = self.sccache_port.clone() else {
            return ServerCheck::NotConfigured;
        };
        self.sccache.last_look = Some(self.generators.clock.monotonic());
        let queue_dir = self.layout.db.parent().unwrap_or(Path::new("."));
        let env = match self.verifier.run_env(queue_dir) {
            Ok(env) => env,
            Err(error) => {
                // Provisioning and the review report the file themselves.
                return ServerCheck::Unconfirmed {
                    port: crate::domain::sccache::DEFAULT_PORT,
                    why: format!("[run.env] could not be read: {error:#}"),
                };
            }
        };
        let Some(target) = SccacheTarget::of_pairs(&env) else {
            return ServerCheck::NotConfigured;
        };
        let unconfirmed = |why: String| ServerCheck::Unconfirmed {
            port: target.port,
            why,
        };
        let running = match server.listening(target.port) {
            Ok(running) => running,
            Err(error) => {
                return unconfirmed(format!(
                    "the sccache server could not be looked at: {error:#}"
                ));
            }
        };
        if !running
            && retry_waiting(
                self.sccache
                    .failed_at
                    .map(|at| self.generators.clock.monotonic().duration_since(at)),
            )
        {
            return unconfirmed(format!(
                "no sccache server listens on port {}, and the last start failed",
                target.port
            ));
        }
        // On the PATH the programs of [run.env] are checked on (ADR-0049
        // decision 9).
        let program = self.verifier.run_env_programs(None).ok().and_then(|check| {
            check
                .programs
                .into_iter()
                .find(|program| program.variable == WRAPPER_VAR)
                .and_then(|program| program.resolved)
        });
        let mut restart =
            crate::application::sccache::pending_restart(&*self.queue, &*server, target.port)
                .unwrap_or_else(|error| {
                    warn!("sccache pending restart could not be read: {error:#}");
                    None
                });
        if running
            && retry_waiting(
                self.sccache
                    .failed_at
                    .map(|at| self.generators.clock.monotonic().duration_since(at)),
            )
        {
            return unconfirmed(
                "the unhealthy sccache server's restart is waiting for retry".into(),
            );
        }
        if running {
            let program = program.as_deref().unwrap_or(&target.program);
            match crate::application::sccache::observe(
                &*self.queue,
                &*server,
                &target,
                Path::new(program),
                &env,
                &mut self.sccache.failures,
                self.generators.clock.now(),
            ) {
                Ok(Some(payload)) => restart = Some(payload),
                Ok(None) if restart.is_some() => {}
                Ok(None) => return ServerCheck::Running,
                Err(error) => {
                    warn!("sccache observation failed: {error:#}");
                    return ServerCheck::Running;
                }
            }
            let mut payload = restart.unwrap();
            payload["supervisor"] = json!(self.token);
            payload["supervisor_pid"] = json!(self.layout.pid);
            payload["at"] = json!(self.generators.clock.now());
            return match crate::application::sccache::restart(
                &*self.queue,
                &*server,
                Path::new(program),
                &env,
                payload,
            ) {
                Ok(()) => {
                    self.sccache.failed_at = None;
                    ServerCheck::Running
                }
                Err(error) => {
                    self.sccache.failed_at = Some(self.generators.clock.monotonic());
                    unconfirmed(format!("sccache restart failed: {error:#}"))
                }
            };
        }
        let started = match &program {
            Some(program) => {
                let mut start_env = env;
                start_env.retain(|(key, _)| key != IDLE_TIMEOUT_VAR);
                start_env.push((IDLE_TIMEOUT_VAR.to_owned(), IDLE_TIMEOUT.to_owned()));
                server.start(Path::new(program), &start_env, target.port)
            }
            None => Err(anyhow!(
                "{WRAPPER_VAR}={} is not found on the supervisor's PATH",
                target.program
            )),
        };
        let mut payload = json!({
            "at": self.generators.clock.now(),
            "port": target.port,
            "program": program.as_deref().unwrap_or(&target.program),
            "supervisor": self.token,
            "supervisor_pid": self.layout.pid,
            "reason": restart.as_ref().map(|_| "restart").unwrap_or(reason.as_str()),
            "replaced": restart,
            "idle_timeout": IDLE_TIMEOUT,
        });
        match started {
            Ok(pid) => {
                self.sccache.failed_at = None;
                match pid {
                    Ok(pid) => {
                        payload["pid"] = json!(pid);
                        info!(
                            port = target.port,
                            "started the sccache server (pid {pid}) on port {} ({})",
                            target.port,
                            reason.as_str()
                        );
                    }
                    // The server runs; the event says why its pid is not
                    // known instead of leaving it out silently.
                    Err(why) => {
                        payload["pid"] = Value::Null;
                        warn!(
                            port = target.port,
                            "started the sccache server on port {} ({}), but its pid could not be read: {why}",
                            target.port,
                            reason.as_str()
                        );
                        payload["pid_error"] = json!(why);
                    }
                }
                if let Ok(Some(process)) = server.process(target.port)
                    && payload["pid"] == process.pid
                {
                    payload["started_at"] = json!(process.started_at);
                    payload["parent_pid"] = json!(process.parent_pid);
                    payload["command"] = json!(process.command);
                }
                if let Err(error) = self
                    .queue
                    .record_queue_event(EventKind::SccacheServerStarted, payload)
                {
                    warn!(error = %format_args!("{error:#}"), "sccache_server_started could not be recorded: {error:#}");
                }
                ServerCheck::Running
            }
            Err(error) => {
                self.sccache.failed_at = Some(self.generators.clock.monotonic());
                let message = format!("{error:#}");
                payload["error"] = json!(message);
                if let Some(replaced) = payload.get("replaced").filter(|v| !v.is_null()) {
                    let mut failure = replaced.clone();
                    failure["error"] = json!(message);
                    failure["at"] = json!(self.generators.clock.now());
                    failure["supervisor"] = json!(self.token);
                    failure["supervisor_pid"] = json!(self.layout.pid);
                    if let Err(error) = crate::application::sccache::record_restart_failure(
                        &*self.queue,
                        Path::new(program.as_deref().unwrap_or(&target.program)),
                        failure,
                    ) {
                        warn!("sccache restart failure could not be recorded: {error:#}");
                    }
                }
                self.record_start_failure(payload);
                unconfirmed(message)
            }
        }
    }

    /// Record `sccache_server_start_failed` unless the latest of the
    /// server's events is the same failure, so a start retried every
    /// [`RETRY_AFTER`] is recorded once until it changes.
    fn record_start_failure(&mut self, payload: Value) {
        warn!(error = %payload["error"], "the sccache server could not be started: {}", payload["error"]);
        let latest = self.queue.latest_queue_event(&[
            crate::domain::sccache::SCCACHE_SERVER_STARTED,
            crate::domain::sccache::SCCACHE_SERVER_START_FAILED,
        ]);
        if let Ok(Some(event)) = &latest
            && event.kind == crate::domain::sccache::SCCACHE_SERVER_START_FAILED
            && event.payload["error"] == payload["error"]
            && event.payload["program"] == payload["program"]
        {
            return;
        }
        if let Err(error) = self
            .queue
            .record_queue_event(EventKind::SccacheServerStartFailed, payload)
        {
            warn!(error = %format_args!("{error:#}"), "sccache_server_start_failed could not be recorded: {error:#}");
        }
    }

    /// Before a Codex job of run `run` given `env` (`[run.env]`) starts:
    /// the variables it runs without. It is given the refusal of the
    /// server's start; when the server was confirmed it compiles through
    /// the guard made in `dir` (ADR-t2008-1), else (or when the guard could
    /// not be made) it runs without `RUSTC_WRAPPER`, recorded as
    /// `sccache_wrapper_removed` and taken out of `env`.
    pub(super) fn sccache_before_job(
        &mut self,
        run: &RunId,
        job: &str,
        attempt: usize,
        dir: &Path,
        env: &mut Vec<(String, String)>,
    ) -> &'static [&'static str] {
        let (port, why, target) = match self.ensure_sccache(CheckReason::BeforeReview) {
            ServerCheck::NotConfigured => return &[],
            ServerCheck::Running => {
                let Some(target) = SccacheTarget::of_pairs(env) else {
                    return &[];
                };
                let guard = self
                    .sccache_port
                    .as_ref()
                    .context("no sccache port")
                    .and_then(|SccachePort(server)| server.guard(dir));
                let why = match guard {
                    Ok(guard) => match guard.to_str() {
                        Some(guard) => {
                            set_env(env, sandbox_env(&target, Some(guard)));
                            return &[];
                        }
                        None => format!("the guard's path {} is not UTF-8", guard.display()),
                    },
                    Err(error) => format!("the guard could not be made: {error:#}"),
                };
                (target.port, why, Some(target))
            }
            ServerCheck::Unconfirmed { port, why } => (port, why, SccacheTarget::of_pairs(env)),
        };
        if let Some(target) = target {
            set_env(env, sandbox_env(&target, None));
        }
        env.retain(|(key, _)| key != WRAPPER_VAR);
        warn!(run_id = %run, "run {run}: its {job} runs without {WRAPPER_VAR}: {why}");
        if let Err(error) = self.queue.record_runtime_event(
            run,
            EventKind::SccacheWrapperRemoved,
            json!({
                "at": self.generators.clock.now(),
                "by": "supervisor",
                "job": job,
                "attempt": attempt,
                "port": port,
                "reason": why,
            }),
        ) {
            warn!(run_id = %run, error = %format_args!("{error:#}"), "sccache_wrapper_removed could not be recorded: {error:#}");
        }
        &[WRAPPER_VAR]
    }
}

/// `env` with each of `vars` in place of the variable of its name.
fn set_env(env: &mut Vec<(String, String)>, vars: Vec<(String, String)>) {
    for (key, value) in vars {
        env.retain(|(name, _)| *name != key);
        env.push((key, value));
    }
}

fn retry_waiting(elapsed: Option<Duration>) -> bool {
    elapsed.is_some_and(|elapsed| elapsed < RETRY_AFTER)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn set_env_replaces_the_variables_of_the_same_name() {
        let mut env = vec![
            ("RUSTC_WRAPPER".to_owned(), "sccache".to_owned()),
            ("PATH".to_owned(), "/bin".to_owned()),
        ];
        set_env(
            &mut env,
            vec![
                ("SCCACHE_ERROR_LOG".to_owned(), "/dev/null/x".to_owned()),
                ("RUSTC_WRAPPER".to_owned(), "/q/guard".to_owned()),
            ],
        );
        assert_eq!(
            env,
            [
                ("PATH".to_owned(), "/bin".to_owned()),
                ("SCCACHE_ERROR_LOG".to_owned(), "/dev/null/x".to_owned()),
                ("RUSTC_WRAPPER".to_owned(), "/q/guard".to_owned()),
            ]
        );
    }

    #[test]
    fn failed_restarts_wait_until_the_retry_boundary() {
        assert!(!retry_waiting(None));
        assert!(retry_waiting(Some(Duration::ZERO)));
        assert!(retry_waiting(Some(RETRY_AFTER - Duration::from_nanos(1))));
        assert!(!retry_waiting(Some(RETRY_AFTER)));
        assert!(!retry_waiting(Some(RETRY_AFTER + Duration::from_secs(1))));
    }
}
