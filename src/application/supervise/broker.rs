//! The queue's resource broker under the supervisor (ADR-t827-3 decisions
//! 2 and 3, [Broker]): with a mode other than `disabled`, the supervisor
//! makes the broker ready on its first pass and before its claims
//! whenever it is not ([`BrokerControl::ensure`]: the machine, the image,
//! the container, the health), and looks at its health every
//! [`BrokerPort::health_interval`]. Every step runs on a job thread, one
//! at a time, so the image's long build never holds a claim; the loop
//! reaps it on a later pass. `preferred` claims whatever the broker's
//! state: a run goes on without it.
//!
//! Three failed looks in a row restart the container once
//! (`auto_repaired`, `repair: broker_restart`, ADR-0047's first layer).
//! A broker that still fails, or could not be made ready (`machine_busy`
//! when a person's machine runs, say), is the queue's attention
//! `broker_unhealthy` with its `reason`, recorded once per failure until
//! the broker runs again (`broker_started`, `broker_healthy`). A failed
//! start is tried again every [`BrokerPort::health_interval`].
//!
//! The supervisor stops the broker only at the end of a drain `down` asked
//! for (`broker_stop_requested` naming its token), and only when no other
//! supervisor of the queue runs: the container, then dagq's machine when
//! no container runs on it ([`BrokerControl::stop`], `broker_stopped` with
//! `by: supervisor`). `up`'s replacement of a supervisor does not ask, and
//! an exec (the handoff of `install` and the automatic update) does not
//! drain, so the broker keeps running through both for the next process. A handoff waits for the job running (an exec would orphan its podman
//! command); a job still running when the loop ends otherwise is left to
//! finish on its own.
//!
//! [Broker]: ../../../docs/design/broker.md

use super::*;
use crate::application::broker::{BrokerControl, BrokerFailure, StartReport};
use crate::domain::broker::{
    BROKER_ATTENTION_KINDS, BROKER_RESTART, BROKER_STOP_REQUESTED, BROKER_UNHEALTHY, BrokerMode,
};

/// How often the health is looked at, and a failed start tried again.
pub const BROKER_HEALTH_INTERVAL: Duration = Duration::from_secs(30);
/// Failed looks in a row before the container is restarted.
pub const BROKER_FAILURES: u32 = 3;

/// What the supervisor drives the queue's broker with.
#[derive(Clone)]
pub struct BrokerPort {
    /// The mode in force at the supervisor's start; a port is given only
    /// for a mode other than `disabled`.
    pub mode: BrokerMode,
    pub control: Arc<dyn BrokerControl>,
    pub health_interval: Duration,
}

/// What a broker job did.
enum JobOutcome {
    Ensured(std::result::Result<StartReport, BrokerFailure>),
    Looked(std::result::Result<(), String>),
    Restarted(std::result::Result<(), BrokerFailure>),
}

impl BrokerWatch {
    /// A start, a look or a restart runs: a handoff waits for it.
    pub(super) const fn running(&self) -> bool {
        self.job.is_some()
    }
}

/// The broker as the supervisor knows it between passes.
#[derive(Default)]
pub(super) struct BrokerWatch {
    job: Option<thread::JoinHandle<JobOutcome>>,
    /// The broker answered its health after the last start.
    ready: bool,
    /// When the last start or look began.
    last: Option<Instant>,
    /// Failed looks in a row.
    failures: u32,
    /// The container was restarted in this run of failures.
    restarted: bool,
}

impl Supervisor<'_> {
    /// Reap the broker's job once it ended; then start the next one when it
    /// is due: a start while the broker is not ready (on the first pass and
    /// before the claims), a look at its health every interval. `working`
    /// is false while the supervisor drains or hands off: it only reaps.
    pub(super) fn broker_pass(&mut self, working: bool) {
        let Some(port) = self.broker_port.clone() else {
            return;
        };
        if let Some(job) = self.broker.job.take() {
            if !job.is_finished() {
                self.broker.job = Some(job);
                return;
            }
            match job.join() {
                Ok(outcome) => self.broker_outcome(&port, outcome),
                Err(_) => warn!("the broker's job panicked"),
            }
        }
        if !working {
            return;
        }
        let restart =
            self.broker.ready && self.broker.failures >= BROKER_FAILURES && !self.broker.restarted;
        let due = self
            .broker
            .last
            .is_none_or(|last| last.elapsed() >= port.health_interval);
        if !restart && !due {
            return;
        }
        self.broker.last = Some(Instant::now());
        let control = port.control.clone();
        self.broker.job = Some(if restart {
            self.broker.restarted = true;
            warn!(
                "the broker's health failed {} times in a row: restarting its container",
                self.broker.failures
            );
            spawn_traced(move || JobOutcome::Restarted(control.restart()))
        } else if self.broker.ready {
            spawn_traced(move || JobOutcome::Looked(control.health()))
        } else {
            info!(mode = port.mode.as_str(), "making the queue's broker ready");
            spawn_traced(move || JobOutcome::Ensured(control.ensure()))
        });
    }

    fn broker_outcome(&mut self, port: &BrokerPort, outcome: JobOutcome) {
        match outcome {
            JobOutcome::Ensured(Ok(report)) => {
                self.broker.ready = true;
                self.broker.failures = 0;
                self.broker.restarted = false;
                info!(
                    port = report.port,
                    "the queue's broker runs on 127.0.0.1:{}", report.port
                );
                if let Some(duration_ms) = report.build_ms {
                    self.record_broker(
                        EventKind::BrokerImageBuilt,
                        json!({"build": self.layout.version, "image": report.image, "duration_ms": duration_ms}),
                    );
                }
                self.record_broker(
                    EventKind::BrokerStarted,
                    json!({
                        "mode": port.mode,
                        "port": report.port,
                        "build": self.layout.version,
                        "image": report.image,
                        "container": report.container,
                        "container_outcome": report.container_outcome,
                        "machine": report.machine,
                    }),
                );
            }
            JobOutcome::Ensured(Err(failure)) => {
                self.broker.ready = false;
                warn!(
                    code = failure.code.as_str(),
                    "the queue's broker could not be made ready: {failure}; runs go on without it"
                );
                self.broker_unhealthy(failure.code.as_str(), &failure.message);
            }
            JobOutcome::Looked(Ok(())) => {
                self.broker.failures = 0;
                self.broker.restarted = false;
                if self.broker_attention_stands() {
                    info!("the queue's broker answers its health again");
                    self.record_broker(EventKind::BrokerHealthy, json!({}));
                }
            }
            JobOutcome::Looked(Err(error)) => {
                self.broker.failures += 1;
                warn!(
                    failures = self.broker.failures,
                    "the queue's broker did not answer its health: {error}"
                );
                if self.broker.restarted && self.broker.failures >= BROKER_FAILURES {
                    self.broker_unhealthy("unhealthy", &error);
                }
            }
            JobOutcome::Restarted(Ok(())) => {
                let failures = self.broker.failures;
                self.broker.failures = 0;
                info!("the queue's broker answers its health after its restart");
                self.record_broker(
                    EventKind::AutoRepaired,
                    json!({
                        "repair": BROKER_RESTART,
                        "layer": "runtime",
                        "conditions": {"failures": failures},
                        "detail": {"container": crate::application::broker::container_name(&self.layout.queue_hash)},
                        "supervisor": self.token,
                    }),
                );
                if self.broker_attention_stands() {
                    self.record_broker(EventKind::BrokerHealthy, json!({}));
                }
            }
            JobOutcome::Restarted(Err(failure)) => {
                warn!(
                    code = failure.code.as_str(),
                    "the queue's broker still fails after its restart: {failure}"
                );
                // Made ready again from the machine up, every interval.
                self.broker.ready = false;
                self.broker_unhealthy(failure.code.as_str(), &failure.message);
            }
        }
    }

    /// At the end of a drain: stop the broker when `down` asked this
    /// supervisor to (the latest `broker_stop_requested` names its token)
    /// and no other supervisor of the queue runs. A job still running is
    /// waited for first, since it holds the queue's broker lock.
    pub(super) fn stop_broker_after_down(&mut self) {
        let Some(port) = self.broker_port.clone() else {
            return;
        };
        let asked = match self.queue.latest_queue_event(&[BROKER_STOP_REQUESTED]) {
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
                warn!(error = %format_args!("{error:#}"), "the broker's stop request could not be read: {error:#}");
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
                "{others} other supervisor(s) of the queue still run: the broker is left to the last one"
            );
            return;
        }
        if let Some(job) = self.broker.job.take()
            && job.join().is_err()
        {
            warn!("the broker's job panicked");
        }
        match port.control.stop() {
            Ok(report) => {
                info!(
                    container_stopped = report.container_stopped,
                    machine_stopped = report.machine_stopped,
                    "the drain `down` asked for is over: the queue's broker is stopped"
                );
                self.record_broker(
                    EventKind::BrokerStopped,
                    json!({
                        "container": crate::application::broker::container_name(&self.layout.queue_hash),
                        "container_stopped": report.container_stopped,
                        "machine_stopped": report.machine_stopped,
                        "by": "supervisor",
                    }),
                );
            }
            Err(failure) => {
                warn!(
                    code = failure.code.as_str(),
                    "the queue's broker could not be stopped after the drain: {failure}"
                );
            }
        }
    }

    /// Whether the latest broker event is `broker_unhealthy`.
    fn broker_attention_stands(&self) -> bool {
        match self.queue.latest_queue_event(&BROKER_ATTENTION_KINDS) {
            Ok(latest) => latest.is_some_and(|event| event.kind == BROKER_UNHEALTHY),
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the broker's events could not be read: {error:#}");
                false
            }
        }
    }

    /// The attention `broker_unhealthy`, unless it already stands: one
    /// failure tells the inbox once, whatever its reasons on the way.
    fn broker_unhealthy(&mut self, reason: &str, message: &str) {
        let latest = match self.queue.latest_queue_event(&BROKER_ATTENTION_KINDS) {
            Ok(latest) => latest,
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the broker's events could not be read: {error:#}");
                return;
            }
        };
        if latest.is_some_and(|event| event.kind == BROKER_UNHEALTHY) {
            return;
        }
        self.record_broker(
            EventKind::BrokerUnhealthy,
            json!({
                "reason": reason,
                "message": message,
                "failures": self.broker.failures,
                "restarted": self.broker.restarted,
            }),
        );
    }

    fn record_broker(&mut self, kind: EventKind, mut payload: Value) {
        payload["supervisor"] = json!(self.token);
        if let Err(error) = self.queue.record_queue_event(kind, payload) {
            warn!(error = %format_args!("{error:#}"), "the broker's {kind} could not be recorded: {error:#}");
        }
    }
}
