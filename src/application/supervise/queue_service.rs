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

impl Supervisor<'_> {
    /// Look at the service when due, start it again or replace it as
    /// needed, and say whether new claims and jobs may start: always
    /// without a port, else whether the service runs. `working` is false
    /// while the supervisor drains or hands off: it only answers.
    pub(super) fn queue_service_pass(&mut self, working: bool) -> bool {
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
            if !self.queue_service.up && self.queue_service_attention_stands() {
                info!("the queue service answers again");
                self.record_queue_service(
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
        self.queue_service
            .starts
            .retain(|start| start.elapsed() < window);
        if !replacing && self.queue_service.starts.len() >= QUEUE_SERVICE_RESTARTS {
            warn!(
                "the queue service was started {} times in {}s and is still not running: left to a person",
                self.queue_service.starts.len(),
                window.as_secs()
            );
            self.queue_service_down(
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
                self.queue_service_down("start_failed", &format!("{error:#}"));
                false
            }
        }
    }

    /// At the end of a drain: stop the service when `down` asked this
    /// supervisor to and no other supervisor of the queue runs.
    pub(super) fn stop_queue_service_after_down(&mut self) {
        let Some(port) = self.queue_service_port.clone() else {
            return;
        };
        let asked = match self
            .queue
            .latest_queue_event(&[QUEUE_SERVICE_STOP_REQUESTED])
        {
            Ok(latest) => latest.is_some_and(|event| {
                event.payload["supervisors"]
                    .as_array()
                    .is_some_and(|tokens| {
                        tokens
                            .iter()
                            .any(|token| token.as_str() == Some(self.token.as_str()))
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
        let now = self.generators.clock.now();
        let others = match self.queue.supervisors() {
            Ok(registrations) => registrations
                .into_iter()
                .filter(|registration| {
                    registration.token != self.token
                        && !heartbeat_stale(
                            self.processes.alive(registration.pid),
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

    fn queue_service_attention_stands(&self) -> bool {
        match self
            .queue
            .latest_queue_event(&QUEUE_SERVICE_ATTENTION_KINDS)
        {
            Ok(latest) => latest.is_some_and(|event| event.kind == QUEUE_SERVICE_DOWN),
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the queue service's events could not be read: {error:#}");
                false
            }
        }
    }

    /// The attention `queue_service_down`, unless it already stands.
    fn queue_service_down(&mut self, reason: &str, message: &str) {
        if self.queue_service_attention_stands() {
            return;
        }
        self.record_queue_service(
            EventKind::QueueServiceDown,
            json!({"reason": reason, "message": message}),
        );
    }

    fn record_queue_service(&mut self, kind: EventKind, mut payload: Value) {
        payload["supervisor"] = json!(self.token);
        if let Err(error) = self.queue.record_queue_event(kind, payload) {
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
