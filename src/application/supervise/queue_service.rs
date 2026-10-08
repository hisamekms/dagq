//! The queue service under the supervisor ([Queue service], ADR-t1233-4
//! decisions 1 to 3): every [`QueueServicePort::interval`] a supervisor at
//! work looks whether the queue's service answers and runs its own build.
//! One that does not answer is started again, at most
//! [`QUEUE_SERVICE_RESTARTS`] times in [`QUEUE_SERVICE_RESTART_WINDOW`],
//! and one of another build (left by the binary before an `install` or an
//! automatic update handed the supervisor over) is replaced, uncounted and
//! once: the build that replacement runs is taken as it is after. Each
//! start is `queue_service_started` (`by: supervisor`, `restart`,
//! `replaced`).
//!
//! While the service is down the supervisor claims no new run and starts
//! no plan review, goal review, observer or throughput review: those would
//! reach the queue through it once they are clients (goal 82's stage
//! (3)). The runs in flight go on. A start that fails, or the limit
//! reached, is the queue's attention `queue_service_down`, written on the
//! DB directly (not through the service) once per failure until the
//! service runs again (`queue_service_started` or
//! `queue_service_running`).
//!
//! A draining or handing-off supervisor does not look: an exec leaves the
//! service to the next process, which replaces it when its build changed.
//! At the end of a drain `down` asked for (`queue_service_stop_requested`
//! naming its token), the last supervisor of the queue stops the service
//! (`queue_service_stopped`, `by: supervisor`).
//!
//! [Queue service]: ../../../docs/design/queue-service.md

use super::*;
use crate::application::queue_service::{QueueServiceControl, ServiceProbe};
use crate::domain::queue_service::{
    QUEUE_SERVICE_ATTENTION_KINDS, QUEUE_SERVICE_DOWN, QUEUE_SERVICE_STOP_REQUESTED, ServiceState,
};

/// How often a supervisor at work looks at the service.
pub const QUEUE_SERVICE_INTERVAL: Duration = Duration::from_secs(10);
/// Starts in [`QUEUE_SERVICE_RESTART_WINDOW`] before the supervisor stops
/// trying and leaves the service to a person.
pub const QUEUE_SERVICE_RESTARTS: usize = 3;
/// The window the starts are counted in.
pub const QUEUE_SERVICE_RESTART_WINDOW: Duration = Duration::from_secs(600);
/// How long a start waits for the service to answer.
pub const QUEUE_SERVICE_START_TIMEOUT: Duration = Duration::from_secs(10);

/// What the supervisor keeps the queue's service with.
#[derive(Clone)]
pub struct QueueServicePort {
    pub control: Arc<dyn QueueServiceControl>,
    pub interval: Duration,
    pub start_timeout: Duration,
}

/// The service as the supervisor knows it between passes.
#[derive(Default)]
pub(super) struct QueueServiceWatch {
    /// When it was last looked at.
    last: Option<Instant>,
    /// It answered, of this build, at the last look.
    up: bool,
    /// It answered at some look of this supervisor's.
    seen: bool,
    /// The starts in the window.
    starts: Vec<Instant>,
    /// The build of the service this supervisor last started: one of
    /// another build than its own is replaced once, not again (its
    /// executable may already be the next binary an `install` put in
    /// place before this process execs it).
    accepted: Option<String>,
}

impl HostOpsState {
    /// Look at the service ([`Self::look_at_queue_service`]) and keep
    /// whether new claims and jobs may start in `service_up`.
    pub(super) fn queue_service_pass(&mut self, env: &mut PassEnv<'_>, working: bool) {
        self.service_up = self.look_at_queue_service(env, working);
    }

    /// Look at the service when due, start it again or replace it as
    /// needed, and say whether new claims and jobs may start: always
    /// without a port, else whether the service runs. `working` is false
    /// while the supervisor drains or hands off: it only answers.
    fn look_at_queue_service(&mut self, env: &mut PassEnv<'_>, working: bool) -> bool {
        let Some(port) = self.queue_service_port.clone() else {
            return true;
        };
        if !working {
            return self.queue_service.up;
        }
        if self
            .queue_service
            .last
            .is_some_and(|last| last.elapsed() < port.interval)
        {
            return self.queue_service.up;
        }
        self.queue_service.last = Some(Instant::now());
        let found = port.control.probe();
        let accepted = found.state == ServiceState::Running
            && found.build.is_some()
            && found.build == self.queue_service.accepted;
        if found.current() || accepted {
            if !self.queue_service.up && self.queue_service_attention_stands(env) {
                info!("the queue service answers again");
                self.record_queue_service(
                    env,
                    EventKind::QueueServiceRunning,
                    json!({"pid": found.pid, "build": found.build}),
                );
            }
            self.queue_service.up = true;
            self.queue_service.seen = true;
            return true;
        }
        // A service of another build that answers is replaced, and the
        // replacement is not counted as a restart.
        let replacing = found.state == ServiceState::Running;
        if !replacing {
            self.queue_service.up = false;
        }
        let window = QUEUE_SERVICE_RESTART_WINDOW;
        let limited = restart_limited(&mut self.queue_service.starts, Instant::now(), window);
        if !replacing && limited {
            warn!(
                "the queue service was started {} times in {}s and is still not running: left to a person",
                self.queue_service.starts.len(),
                window.as_secs()
            );
            self.queue_service_down(
                env,
                "restart_limit",
                &format!(
                    "started {} times in {}s; {}",
                    self.queue_service.starts.len(),
                    window.as_secs(),
                    describe(&found)
                ),
            );
            return false;
        }
        if !replacing {
            self.queue_service.starts.push(Instant::now());
        }
        let replaced = (found.state != ServiceState::Stopped).then(|| found.clone());
        match port.control.start(port.start_timeout) {
            Ok(started) => {
                info!(
                    pid = started.pid,
                    "the queue service runs at {}",
                    started.socket.display()
                );
                self.record_queue_service(
                    env,
                    EventKind::QueueServiceStarted,
                    json!({
                        "by": "supervisor",
                        "pid": started.pid,
                        "build": started.build,
                        "api_version": started.api_version,
                        "socket": started.socket,
                        "restart": self.queue_service.seen,
                        "replaced": replaced,
                    }),
                );
                self.queue_service.accepted = started.build.clone();
                self.queue_service.up = true;
                self.queue_service.seen = true;
                true
            }
            Err(error) => {
                self.queue_service.up = false;
                warn!(error = %format_args!("{error:#}"), "the queue service could not be started: {error:#}; no new run is claimed meanwhile");
                self.queue_service_down(env, "start_failed", &format!("{error:#}"));
                false
            }
        }
    }

    /// At the end of a drain: stop the service when `down` asked this
    /// supervisor to and no other supervisor of the queue runs.
    pub(super) fn stop_queue_service_after_down(&mut self, env: &mut PassEnv<'_>) {
        let Some(port) = self.queue_service_port.clone() else {
            return;
        };
        let asked = match env
            .queue
            .latest_queue_event(&[QUEUE_SERVICE_STOP_REQUESTED])
        {
            Ok(latest) => latest.is_some_and(|event| {
                event.payload["supervisors"]
                    .as_array()
                    .is_some_and(|tokens| {
                        tokens
                            .iter()
                            .any(|token| token.as_str() == Some(env.token.as_str()))
                    })
            }),
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the queue service's stop request could not be read: {error:#}");
                false
            }
        };
        if !asked {
            return;
        }
        let now = env.generators.clock.now();
        let others = match env.queue.supervisors() {
            Ok(registrations) => registrations
                .into_iter()
                .filter(|registration| {
                    registration.token != *env.token
                        && !heartbeat_stale(
                            env.processes.alive(registration.pid),
                            now - registration.heartbeat_at,
                        )
                })
                .count(),
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the queue's supervisors could not be read: {error:#}");
                return;
            }
        };
        if others > 0 {
            info!(
                "{others} other supervisor(s) of the queue still run: the queue service is left to the last one"
            );
            return;
        }
        match port.control.stop(port.start_timeout) {
            Ok(Some(pid)) => {
                info!(
                    pid,
                    "the drain `down` asked for is over: the queue service is stopped"
                );
                self.record_queue_service(
                    env,
                    EventKind::QueueServiceStopped,
                    json!({"pid": pid, "by": "supervisor"}),
                );
            }
            Ok(None) => {}
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the queue service could not be stopped after the drain: {error:#}");
            }
        }
    }

    fn queue_service_attention_stands(&mut self, env: &mut PassEnv<'_>) -> bool {
        match env.queue.latest_queue_event(&QUEUE_SERVICE_ATTENTION_KINDS) {
            Ok(latest) => latest.is_some_and(|event| event.kind == QUEUE_SERVICE_DOWN),
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the queue service's events could not be read: {error:#}");
                false
            }
        }
    }

    /// The attention `queue_service_down`, unless it already stands.
    fn queue_service_down(&mut self, env: &mut PassEnv<'_>, reason: &str, message: &str) {
        if self.queue_service_attention_stands(env) {
            return;
        }
        self.record_queue_service(
            env,
            EventKind::QueueServiceDown,
            json!({"reason": reason, "message": message}),
        );
    }

    fn record_queue_service(&mut self, env: &mut PassEnv<'_>, kind: EventKind, mut payload: Value) {
        payload["supervisor"] = json!(env.token);
        if let Err(error) = env.queue.record_queue_event(kind, payload) {
            warn!(error = %format_args!("{error:#}"), "the queue service's {kind} could not be recorded: {error:#}");
        }
    }
}

/// What a look found, for the attention's message.
fn describe(found: &ServiceProbe) -> String {
    match &found.error {
        Some(error) => format!("{}: {error}", found.state.as_str()),
        None => found.state.as_str().to_owned(),
    }
}

/// Keep the `starts` still in `window` at `at`, and say whether they reach
/// [`QUEUE_SERVICE_RESTARTS`]: the service is then left to a person.
fn restart_limited(starts: &mut Vec<Instant>, at: Instant, window: Duration) -> bool {
    starts.retain(|start| at.saturating_duration_since(*start) < window);
    starts.len() >= QUEUE_SERVICE_RESTARTS
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The starts are counted in the window up to 1 ms before its end: at
    /// its end the oldest leaves it and the service is started again.
    #[test]
    fn the_restart_limit_counts_the_starts_in_the_window_only() {
        let window = QUEUE_SERVICE_RESTART_WINDOW;
        let base = Instant::now();
        let starts = || {
            (0..QUEUE_SERVICE_RESTARTS as u64)
                .map(|n| base + Duration::from_secs(n))
                .collect::<Vec<_>>()
        };
        let mut held = starts();
        assert!(restart_limited(
            &mut held,
            base + window - Duration::from_millis(1),
            window
        ));
        assert_eq!(held.len(), QUEUE_SERVICE_RESTARTS);
        let mut freed = starts();
        assert!(!restart_limited(&mut freed, base + window, window));
        assert_eq!(freed.len(), QUEUE_SERVICE_RESTARTS - 1);
        assert!(!restart_limited(&mut Vec::new(), base, window));
    }
}
