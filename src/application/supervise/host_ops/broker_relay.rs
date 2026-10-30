//! The relays of the runs' containers under the supervisor ([Container
//! runtime], ADR-t2114-3 decision 4): every [`QueueRelayPort::interval`] a
//! supervisor at work looks at each relay the queue keeps open while the
//! queue broker answers. A relay that does not run, or passes to a port
//! the broker left, is started again towards the broker's port; one that
//! runs and does not reach the broker, or a look whose podman does not
//! reach the machine (both are the machine's gvproxy), has the dagq
//! machine started again under its host-wide lock first, and then every
//! relay.
//! At most [`QUEUE_SERVICE_RESTARTS`] such starts in
//! [`QUEUE_SERVICE_RESTART_WINDOW`] are made, each `queue_relay_started`.
//! A look or a start that fails, or the limit reached, is the queue's
//! attention `queue_relay_down`, once until every relay runs again
//! (`queue_relay_running`); meanwhile the container runtime cannot be used
//! ([`crate::application::broker_relay::RelayGate`]), so no new run is set
//! to it.
//!
//! A relay whose run is over (its task is not in progress, a later run
//! took its place, it landed, or the queue has no such run) is closed
//! instead
//! ([`crate::application::broker_relay::close`]): the control side keeps
//! its record until a close succeeds, so a supervisor that went down or a
//! close that failed at the run's end leaves it. It is not started again
//! and counts for no start and no attention; a close that fails, or one
//! while the machine is stopped, is tried again at a later look. Once a
//! close is out of its time ([`RelayTimedOut`]), the other relays of runs
//! that are over are not closed in that pass but at the next look. A
//! relay whose run may be resumed but uses no machine meanwhile is idle:
//! kept, and neither looked at, started again nor closed.
//!
//! After the closes, at every look while no relay is to be looked at and
//! the machine runs, the dagq machine stops when no user of any queue is
//! held ([`may_stop_machine`]): a run that ended is no longer counted,
//! whether or not it ever recorded a relay, and a stop that failed is
//! tried again at the next look.
//!
//! A broker that does not answer is the broker's watch's
//! ([`super::queue_broker`]): the relays are not looked at meanwhile. A
//! draining or handing-off supervisor does not look.
//!
//! The judgement of each relay is [`relay_fate`], [`may_stop_machine`] and [`relay_action`] in
//! the domain, the
//! window's [`in_window`], given the monotonic time of the injected clock
//! as values.
//!
//! [Container runtime]: ../../../docs/design/container-runtime.md

use super::*;
use crate::application::RunNotFound;
use crate::application::broker_relay::{RelayControl, RelayTimedOut, close};
use crate::application::queue_broker::QueueBrokerControl;
use crate::domain::broker_relay::{
    QUEUE_RELAY_ATTENTION_KINDS, RelayAction, RelayFate, RelayRun, RelaySeen, address_port,
    may_stop_machine, relay_action, relay_fate,
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
pub(in crate::application::supervise) struct QueueRelayWatch {
    /// When they were last looked at.
    last: Option<Instant>,
    /// The starts (of a relay, or of the machine) in the window.
    starts: Vec<Instant>,
}

impl HostOpsState {
    /// Look at the relays when due, and start them or the machine again as
    /// needed. `working` is false while the supervisor drains or hands
    /// off: it does not look.
    pub(in crate::application::supervise) fn broker_relay_pass(
        &mut self,
        env: &mut HostEnv<'_>,
        working: bool,
    ) {
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
        let seen = self.close_relays_of_runs_over(env, port.relays.as_ref(), seen);
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
                "a relay does not reach the queue broker, or podman does not reach the dagq machine: the machine is started again"
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

    /// Close the relays in `seen` whose run is over, leave the idle ones
    /// alone, and give back those to be looked at. Then the dagq machine
    /// stops if no user is held ([`stop_unused_machine`]).
    ///
    /// This is where a run in the container stops being what keeps the
    /// machine running: its relay
    /// is opened before its first turn and kept between its turns, and the
    /// watch is what finds its end, whichever way it ended (it landed, its
    /// task was completed or canceled, a later run took its place, or it
    /// failed or was interrupted and waits for its triage or an answer).
    /// The session wrapper's end is not: a run waiting for an answer
    /// between its turns has no wrapper and still uses the machine. A run
    /// that ended before it recorded a relay (its image or relay did not
    /// start after the machine was made to run for it) has nothing here to
    /// find, so the stop does not wait for a relay's end: it is tried at
    /// every look while no relay is to be looked at. A relay's close needs
    /// the machine running; while it is stopped, the close waits for it to
    /// run again (nothing of the run runs in a stopped machine, and its
    /// volumes stay in it).
    fn close_relays_of_runs_over(
        &mut self,
        env: &mut HostEnv<'_>,
        relays: &dyn RelayControl,
        seen: Vec<RelaySeen>,
    ) -> Vec<RelaySeen> {
        let in_progress = match env.queue.latest_runs_in_progress() {
            Ok(runs) => Some(runs),
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the runs of the relays could not be read: {error:#}");
                None
            }
        };
        let fated: Vec<(RelaySeen, RelayFate)> = seen
            .into_iter()
            .map(|relay| {
                let fate = relay_fate(relay_run(env, in_progress.as_deref(), &relay.run));
                (relay, fate)
            })
            .collect();
        let fates: Vec<RelayFate> = fated.iter().map(|(_, fate)| *fate).collect();
        let stops = may_stop_machine(&fates);
        let closes = fates.contains(&RelayFate::Close);
        let machine_runs = if stops || closes {
            relays.machine_runs()
        } else {
            Ok(true)
        };
        // A machine that cannot be asked is taken to run for a close, which
        // tells; it is not stopped (a host without podman asks at every
        // look, unwarned).
        let stops = stops && matches!(machine_runs, Ok(true));
        let machine_runs = machine_runs.unwrap_or_else(|error| {
            if closes {
                warn!(error = %format_args!("{error:#}"), "whether the dagq machine runs could not be read: {error:#}");
            }
            true
        });
        // Once a close is out of time, the rest are not tried in this pass:
        // each would wait as long again before the machine is started again.
        let mut timed_out = false;
        let looked = fated
            .into_iter()
            .filter_map(|(relay, fate)| {
                match fate {
                    RelayFate::Look => return Some(relay),
                    RelayFate::Idle => return None,
                    RelayFate::Close => {}
                }
                if timed_out || !machine_runs {
                    return None;
                }
                match close(relays, &relay.run) {
                    Ok(()) => info!(run = relay.run, "the relay of a run that is over is closed"),
                    Err(error) => {
                        timed_out = error.downcast_ref::<RelayTimedOut>().is_some();
                        warn!(run = relay.run, error = %format_args!("{error:#}"), "the relay of a run that is over could not be closed: {error:#}; tried again at the next look");
                    }
                }
                None
            })
            .collect();
        if stops {
            stop_unused_machine(relays);
        }
        looked
    }

    fn broker_relay_attention_stands(&mut self, env: &mut HostEnv<'_>) -> bool {
        match env.queue.latest_queue_event(&QUEUE_RELAY_ATTENTION_KINDS) {
            Ok(latest) => latest.is_some_and(|event| event.kind == EventKind::QueueRelayDown),
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

/// After every close of the pass (each close needs the machine), stop the
/// running dagq machine if no user of any queue is held. No run is made to
/// leave by its ID: a run's lease that is no longer held is forgotten as
/// the users are counted, while one that resumed and joined again
/// meanwhile keeps the machine. A stop that fails is warned of and tried
/// again at the next look.
fn stop_unused_machine(relays: &dyn RelayControl) {
    match relays.stop_unused() {
        Ok(true) => info!("no user of the dagq machine is left: it is stopped"),
        Ok(false) => {}
        Err(error) => {
            warn!(error = %format_args!("{error:#}"), "the unused dagq machine could not be stopped: {error:#}; tried again at the next look");
        }
    }
}

/// What the queue says of the run `run`, given the latest runs of the tasks
/// in progress (`None` when they could not be read).
fn relay_run(env: &mut HostEnv<'_>, in_progress: Option<&[TaskRun]>, run: &str) -> RelayRun {
    let Some(in_progress) = in_progress else {
        return RelayRun::Unknown;
    };
    if let Some(found) = in_progress.iter().find(|found| found.id().as_str() == run) {
        return RelayRun::Found {
            status: found.status(),
            task_in_progress: true,
        };
    }
    let Ok(id) = RunId::new(run) else {
        return RelayRun::Missing;
    };
    match env.queue.run(&id) {
        Ok(found) => RelayRun::Found {
            status: found.status(),
            task_in_progress: false,
        },
        Err(error) if error.downcast_ref::<RunNotFound>().is_some() => RelayRun::Missing,
        Err(error) => {
            warn!(run, error = %format_args!("{error:#}"), "the run of a relay could not be read: {error:#}");
            RelayRun::Unknown
        }
    }
}
