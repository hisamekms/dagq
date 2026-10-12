//! The relays of the runs' containers under the supervisor ([Container
//! runtime], ADR-t2114-3 decision 4): every [`QueueRelayPort::interval`] a
//! supervisor at work looks at each relay the queue keeps open while the
//! queue broker answers. A relay that does not run, or passes to a port
//! the broker left, is started again towards the broker's port; one that
//! runs and does not reach the broker (the machine's gvproxy) has the dagq
//! machine started again under its host-wide lock, and then every relay.
//! At most [`QUEUE_SERVICE_RESTARTS`] such starts in
//! [`QUEUE_SERVICE_RESTART_WINDOW`] are made, each `queue_relay_started`.
//! A look or a start that fails, or the limit reached, is the queue's
//! attention `queue_relay_down`, once until every relay runs again
//! (`queue_relay_running`); meanwhile the container runtime cannot be used
//! ([`crate::application::broker_relay::RelayGate`]), so no new run is set
//! to it.
//!
//! A broker that does not answer is the broker's watch's
//! ([`super::queue_broker`]): the relays are not looked at meanwhile. A
//! draining or handing-off supervisor does not look.
//!
//! The judgement of each relay is [`relay_action`] in the domain, the
//! window's [`in_window`], given the monotonic time of the injected clock
//! as values.
//!
//! [Container runtime]: ../../../docs/design/container-runtime.md

use super::*;
use crate::application::broker_relay::RelayControl;
use crate::application::queue_broker::QueueBrokerControl;
use crate::domain::broker_relay::{
    QUEUE_RELAY_ATTENTION_KINDS, QUEUE_RELAY_DOWN, RelayAction, address_port, relay_action,
};
use crate::domain::queue_broker::{in_window, look_due};
use crate::domain::queue_service::ServiceState;

/// What the supervisor keeps the runs' relays with.
#[derive(Clone)]
pub struct QueueRelayPort {
    pub relays: Arc<dyn RelayControl>,
    /// The queue's broker, whose port the relays pass to.
    pub broker: Arc<dyn QueueBrokerControl>,
    pub interval: Duration,
}

/// The relays as the supervisor knows them between passes.
#[derive(Default)]
pub(super) struct QueueRelayWatch {
    /// When they were last looked at.
    last: Option<Instant>,
    /// The starts (of a relay, or of the machine) in the window.
    starts: Vec<Instant>,
}

impl HostOpsState {
    /// Look at the relays when due, and start them or the machine again as
    /// needed. `working` is false while the supervisor drains or hands
    /// off: it does not look.
    pub(super) fn broker_relay_pass(&mut self, env: &mut HostEnv<'_>, working: bool) {
        let Some(port) = self.broker_relay_port.clone() else {
            return;
        };
        let now = env.generators.clock.monotonic();
        let since_last = self
            .broker_relay
            .last
            .map(|last| now.saturating_duration_since(last));
        if !working || !look_due(since_last, port.interval) {
            return;
        }
        self.broker_relay.last = Some(now);
        let found = port.broker.probe();
        let Some(broker_port) = found
            .address
            .as_deref()
            .and_then(address_port)
            .filter(|_| found.state == ServiceState::Running)
        else {
            return;
        };
        let seen = match port.relays.relays() {
            Ok(seen) => seen,
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the runs' relays could not be looked at: {error:#}");
                self.broker_relay_down(env, "look_failed", &format!("{error:#}"));
                return;
            }
        };
        let window = QUEUE_SERVICE_RESTART_WINDOW;
        self.broker_relay
            .starts
            .retain(|start| in_window(now.saturating_duration_since(*start), window));
        let actions: Vec<(&str, RelayAction)> = seen
            .iter()
            .map(|relay| (relay.run.as_str(), relay_action(relay, broker_port)))
            .collect();
        let machine = actions
            .iter()
            .any(|(_, action)| *action == RelayAction::RestartMachine);
        let restart: Vec<&str> = actions
            .iter()
            .filter(|(_, action)| machine || *action == RelayAction::Restart)
            .map(|(run, _)| *run)
            .collect();
        if restart.is_empty() {
            if self.broker_relay_attention_stands(env) {
                info!("the runs' relays run again");
                self.record_broker_relay(
                    env,
                    EventKind::QueueRelayRunning,
                    json!({"relays": seen.len()}),
                );
            }
            return;
        }
        if self.broker_relay.starts.len() >= QUEUE_SERVICE_RESTARTS {
            let message = format!(
                "started {} times in {}s; still down: {}",
                self.broker_relay.starts.len(),
                window.as_secs(),
                restart.join(", ")
            );
            warn!("the runs' relays are still not running: left to a person ({message})");
            self.broker_relay_down(env, "restart_limit", &message);
            return;
        }
        self.broker_relay.starts.push(now);
        if machine {
            warn!(
                "a relay runs and does not reach the queue broker: the dagq machine is started again"
            );
            if let Err(error) = port.relays.restart_machine() {
                warn!(error = %format_args!("{error:#}"), "the dagq machine could not be started again: {error:#}");
                self.broker_relay_down(
                    env,
                    "start_failed",
                    &format!("start the dagq machine again: {error:#}"),
                );
                return;
            }
        }
        for run in restart {
            match port.relays.restart(run, broker_port) {
                Ok(()) => {
                    info!(run, "the run's relay runs again");
                    self.record_broker_relay(
                        env,
                        EventKind::QueueRelayStarted,
                        json!({"run": run, "port": broker_port, "machine_restarted": machine}),
                    );
                }
                Err(error) => {
                    warn!(run, error = %format_args!("{error:#}"), "the run's relay could not be started again: {error:#}; the container runtime cannot be used meanwhile");
                    self.broker_relay_down(
                        env,
                        "start_failed",
                        &format!("the relay of run {run}: {error:#}"),
                    );
                    return;
                }
            }
        }
    }

    fn broker_relay_attention_stands(&mut self, env: &mut HostEnv<'_>) -> bool {
        match env.queue.latest_queue_event(&QUEUE_RELAY_ATTENTION_KINDS) {
            Ok(latest) => latest.is_some_and(|event| event.kind == QUEUE_RELAY_DOWN),
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the relays' events could not be read: {error:#}");
                false
            }
        }
    }

    /// The attention `queue_relay_down`, unless it already stands.
    fn broker_relay_down(&mut self, env: &mut HostEnv<'_>, reason: &str, message: &str) {
        if self.broker_relay_attention_stands(env) {
            return;
        }
        self.record_broker_relay(
            env,
            EventKind::QueueRelayDown,
            json!({"reason": reason, "message": message}),
        );
    }

    fn record_broker_relay(&mut self, env: &mut HostEnv<'_>, kind: EventKind, mut payload: Value) {
        payload["supervisor"] = json!(env.token);
        if let Err(error) = env.queue.record_queue_event(kind, payload) {
            warn!(error = %format_args!("{error:#}"), "the relays' {kind} could not be recorded: {error:#}");
        }
    }
}
