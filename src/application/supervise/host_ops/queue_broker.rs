//! The queue broker under the supervisor ([Queue broker], ADR-t2114-1
//! decision 1): kept as the queue service is ([`super::queue_service`]),
//! by the same steps and limits. Every [`QueueBrokerPort::interval`] a
//! supervisor at work looks whether the broker answers and runs its own
//! build; one that does not answer is started again, at most
//! [`QUEUE_SERVICE_RESTARTS`] times in [`QUEUE_SERVICE_RESTART_WINDOW`],
//! and one of another build (left by the binary before an `install` or an
//! automatic update handed the supervisor over) is replaced, uncounted and
//! once. Each start is `queue_broker_started` (`by: supervisor`,
//! `restart`, `replaced`). A start that fails, or the limit reached, is
//! the queue's attention `queue_broker_down`, once per failure until the
//! broker runs again.
//!
//! Unlike the service's, a broker that is down holds no claim and no job
//! here: only a run in a container goes out through it, and the container
//! runtime answers that it cannot be used while the broker does not run
//! ([`crate::application::queue_broker::BrokerGate`]), which holds what is
//! set to the container ([`super::container`]).
//!
//! A draining or handing-off supervisor does not look, and the last one
//! of the queue stops the broker at the end of the drain `down` asked for
//! (`queue_broker_stop_requested` naming its token).
//!
//! The judgement of each look (whether it is due, whether a start is in
//! the window, and the next step) is [`watch_step`] and its kin in the
//! domain, given the monotonic time of the injected clock as values.
//!
//! [Queue broker]: ../../../docs/design/queue-broker.md

use super::*;
use crate::application::queue_broker::QueueBrokerControl;
use crate::domain::queue_broker::{
    BrokerSeen, QUEUE_BROKER_ATTENTION_KINDS, QUEUE_BROKER_DOWN, QUEUE_BROKER_STOP_REQUESTED,
    WatchStep, in_window, look_due, watch_step,
};
use crate::domain::queue_service::ServiceState;

/// What the supervisor keeps the queue's broker with.
#[derive(Clone)]
pub struct QueueBrokerPort {
    pub control: Arc<dyn QueueBrokerControl>,
    pub interval: Duration,
    pub start_timeout: Duration,
}

/// The broker as the supervisor knows it between passes.
#[derive(Default)]
pub(in crate::application::supervise) struct QueueBrokerWatch {
    /// When it was last looked at.
    last: Option<Instant>,
    /// It answered, of an accepted build, at the last look.
    up: bool,
    /// It answered at some look of this supervisor's.
    seen: bool,
    /// The starts in the window.
    starts: Vec<Instant>,
    /// The build of the broker this supervisor last started, taken as it
    /// is after (as the queue service's).
    accepted: Option<String>,
}

impl HostOpsState {
    /// Look at the broker when due, and start it again or replace it as
    /// needed. `working` is false while the supervisor drains or hands
    /// off: it does not look.
    pub(in crate::application::supervise) fn queue_broker_pass(
        &mut self,
        env: &mut HostEnv<'_>,
        working: bool,
    ) {
        let Some(port) = self.queue_broker_port.clone() else {
            return;
        };
        let now = env.generators.clock.monotonic();
        let since_last = self
            .queue_broker
            .last
            .map(|last| now.saturating_duration_since(last));
        if !working || !look_due(since_last, port.interval) {
            return;
        }
        self.queue_broker.last = Some(now);
        let found = port.control.probe();
        let window = QUEUE_SERVICE_RESTART_WINDOW;
        self.queue_broker
            .starts
            .retain(|start| in_window(now.saturating_duration_since(*start), window));
        let step = watch_step(
            &BrokerSeen {
                state: found.state,
                build: found.build.as_deref(),
                build_matches: found.build_matches,
            },
            self.queue_broker.accepted.as_deref(),
            self.queue_broker.starts.len(),
            QUEUE_SERVICE_RESTARTS,
        );
        let replacing = match step {
            WatchStep::Keep => {
                if !self.queue_broker.up && self.queue_broker_attention_stands(env) {
                    info!("the queue broker answers again");
                    self.record_queue_broker(
                        env,
                        EventKind::QueueBrokerRunning,
                        json!({"pid": found.pid, "build": found.build}),
                    );
                }
                self.queue_broker.up = true;
                self.queue_broker.seen = true;
                return;
            }
            WatchStep::LeaveToPerson => {
                self.queue_broker.up = false;
                let message = format!(
                    "started {} times in {}s; {}",
                    self.queue_broker.starts.len(),
                    window.as_secs(),
                    found.describe()
                );
                warn!("the queue broker is still not running: left to a person ({message})");
                self.queue_broker_down(env, "restart_limit", &message);
                return;
            }
            WatchStep::Start { replacing } => replacing,
        };
        if !replacing {
            self.queue_broker.up = false;
            self.queue_broker.starts.push(now);
        }
        let replaced = (found.state != ServiceState::Stopped).then_some(found);
        match port.control.start(port.start_timeout) {
            Ok(started) => {
                info!(pid = started.pid, address = ?started.address, "the queue broker runs");
                let mut payload = crate::application::queue_broker::started_payload(
                    "supervisor",
                    &json!({"broker": started, "replaced": replaced}),
                );
                payload["restart"] = json!(self.queue_broker.seen);
                self.record_queue_broker(env, EventKind::QueueBrokerStarted, payload);
                self.queue_broker.accepted = started.build;
                self.queue_broker.up = true;
                self.queue_broker.seen = true;
            }
            Err(error) => {
                self.queue_broker.up = false;
                warn!(error = %format_args!("{error:#}"), "the queue broker could not be started: {error:#}; the container runtime cannot be used meanwhile");
                self.queue_broker_down(env, "start_failed", &format!("{error:#}"));
            }
        }
    }

    /// At the end of a drain: stop the broker when `down` asked this
    /// supervisor to and no other supervisor of the queue runs.
    pub(in crate::application::supervise) fn stop_queue_broker_after_down(
        &mut self,
        env: &mut HostEnv<'_>,
    ) {
        let Some(port) = self.queue_broker_port.clone() else {
            return;
        };
        if !self.last_supervisor_asked(env, QUEUE_BROKER_STOP_REQUESTED) {
            return;
        }
        match port.control.stop(port.start_timeout) {
            Ok(Some(pid)) => {
                info!(
                    pid,
                    "the drain `down` asked for is over: the queue broker is stopped"
                );
                self.record_queue_broker(
                    env,
                    EventKind::QueueBrokerStopped,
                    json!({"pid": pid, "by": "supervisor"}),
                );
            }
            Ok(None) => {}
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the queue broker could not be stopped after the drain: {error:#}");
            }
        }
    }

    fn queue_broker_attention_stands(&mut self, env: &mut HostEnv<'_>) -> bool {
        match env.queue.latest_queue_event(&QUEUE_BROKER_ATTENTION_KINDS) {
            Ok(latest) => latest.is_some_and(|event| event.kind == QUEUE_BROKER_DOWN),
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the queue broker's events could not be read: {error:#}");
                false
            }
        }
    }

    /// The attention `queue_broker_down`, unless it already stands.
    fn queue_broker_down(&mut self, env: &mut HostEnv<'_>, reason: &str, message: &str) {
        if self.queue_broker_attention_stands(env) {
            return;
        }
        self.record_queue_broker(
            env,
            EventKind::QueueBrokerDown,
            json!({"reason": reason, "message": message}),
        );
    }

    fn record_queue_broker(&mut self, env: &mut HostEnv<'_>, kind: EventKind, mut payload: Value) {
        payload["supervisor"] = json!(env.token);
        if let Err(error) = env.queue.record_queue_event(kind, payload) {
            warn!(error = %format_args!("{error:#}"), "the queue broker's {kind} could not be recorded: {error:#}");
        }
    }
}
