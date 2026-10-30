//! The relays of the runs' containers ([Container runtime], ADR-t2114-3
//! decisions 2 and 4): how the control side opens a run's socket and relay
//! in the dagq machine before the run's container starts and closes them
//! at the run's end ([`open`], [`close`], through [`RelayControl`]), with
//! the dagq machine stopped by the supervisor's watch while no relay is
//! to be looked at and no user is left ([`RelayControl::stop_unused`]),
//! and
//! how the container runtime cannot be used while the supervisor could not
//! keep the relays running ([`RelayGate`]). The supervisor's watch is
//! [`super::supervise`]'s.
//!
//! What is decided is [`crate::domain::broker_relay`]'s; the podman
//! commands are the adapters'.
//!
//! [Container runtime]: ../../docs/design/container-runtime.md

use std::fmt;
use std::sync::Arc;

use anyhow::{Context, Result, ensure};

use super::EventStore;
use super::queue_broker::QueueBrokerControl;
use crate::domain::broker_relay::{
    QUEUE_RELAY_ATTENTION_KINDS, RelayRun, RelaySeen, RelaySpec, address_port, attention_stands,
    relay_serves,
};
use crate::domain::container_runtime::{Availability, RuntimeTarget};
use crate::domain::queue_service::ServiceState;

/// The relays of one queue in the dagq machine, each named by its run.
pub trait RelayControl: Send + Sync {
    /// Make the run's socket volume and start its relay towards the
    /// broker's `port`, and remember it among the queue's relays.
    fn open(&self, spec: &RelaySpec, port: u16) -> Result<()>;
    /// The run's end in the machine: stop and remove its turn's container
    /// and its relay, remove its socket and build output volumes, and
    /// forget the relay; what is not there is no error. A podman command
    /// that did not end within its limit fails with [`RelayTimedOut`] in
    /// the error's chain.
    fn close(&self, run: &str) -> Result<()>;
    /// Each relay the queue keeps open, as a look finds it: a relay whose
    /// container went (the machine stopped) is one that does not run.
    fn relays(&self) -> Result<Vec<RelaySeen>>;
    /// Start the run's relay again towards the broker's `port`, in place
    /// of the one there.
    fn restart(&self, run: &str, port: u16) -> Result<()>;
    /// Stop and start the dagq machine under its host-wide lock: what
    /// mends its gvproxy (ADR-t2114-3 decision 4).
    fn restart_machine(&self) -> Result<()>;
    /// Whether the dagq machine runs now: a relay's [`Self::close`] needs
    /// it, and what a stopped machine keeps (the volumes) waits for it to
    /// run again.
    fn machine_runs(&self) -> Result<bool>;
    /// Stop the dagq machine when it runs and no user of any queue is
    /// held, the runs that ended forgotten as they are counted
    /// ([`super::podman_machine::stop_unused`]). Whether it was stopped is
    /// returned.
    fn stop_unused(&self) -> Result<bool>;
}

/// A podman command of a relay's start or close that did not end within
/// its limit and was stopped: the machine does not answer, so another
/// command now would wait as long.
#[derive(Debug)]
pub struct RelayTimedOut(pub String);

impl fmt::Display for RelayTimedOut {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RelayTimedOut {}

/// What the gate reads of the supervisor's watch: the message of the
/// relays' attention while it stands.
pub trait RelayRecords: Send + Sync {
    fn down(&self) -> Result<Option<String>>;
}

/// The message of the relays' attention (`queue_relay_down`) while it
/// stands in `events`.
pub fn down_message(events: &dyn EventStore) -> Result<Option<String>> {
    Ok(events
        .latest_queue_event(&QUEUE_RELAY_ATTENTION_KINDS)?
        .filter(|event| attention_stands(Some(&event.kind)))
        .map(|event| {
            event
                .payload
                .get("message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("down")
                .to_owned()
        }))
}

/// Before the run's container starts: open its socket and relay towards
/// the broker's port. A broker that does not run, or a relay that does not
/// start, is an error, and the run does not start (fail closed).
pub fn open(
    control: &dyn RelayControl,
    broker: &dyn QueueBrokerControl,
    spec: &RelaySpec,
) -> Result<()> {
    let port = broker_port(broker)?;
    control
        .open(spec, port)
        .with_context(|| format!("open the relay of run {}", spec.run))
}

/// Before a turn of the run: keep the relay recorded for it when it
/// serves the turn as it is, `seen` being what a look found of it (`None`
/// when none is recorded), else open it (again) towards the broker's port
/// now ([`relay_serves`]). A broker that does not run is an error, as for
/// [`open`].
pub fn ready_for_turn(
    control: &dyn RelayControl,
    broker: &dyn QueueBrokerControl,
    spec: &RelaySpec,
    seen: Option<&RelaySeen>,
) -> Result<()> {
    let port = broker_port(broker)?;
    if relay_serves(seen, port) {
        return Ok(());
    }
    control
        .open(spec, port)
        .with_context(|| format!("open the relay of run {}", spec.run))
}

/// The port of the queue broker that runs.
fn broker_port(broker: &dyn QueueBrokerControl) -> Result<u16> {
    let found = broker.probe();
    ensure!(
        found.state == ServiceState::Running,
        "the queue broker is not running ({})",
        found.describe()
    );
    found
        .address
        .as_deref()
        .and_then(address_port)
        .context("the queue broker recorded no port")
}

/// At the run's end: close its relay, remove its socket and its build
/// output, and stop its container if one is left.
pub fn close(control: &dyn RelayControl, run: &str) -> Result<()> {
    control
        .close(run)
        .with_context(|| format!("close the relay of run {run}"))
}

/// The container runtime as it can be used with the runs' relays: while
/// the supervisor could not keep them running (the attention stands, or
/// it cannot be read), no target set to the container can run, and the
/// answer says why; else `container` answers.
pub struct RelayGate<C> {
    pub records: Arc<dyn RelayRecords>,
    pub container: C,
}

impl<C: super::ContainerRuntime> super::ContainerRuntime for RelayGate<C> {
    fn availability(&self, target: RuntimeTarget) -> Availability {
        match self.records.down() {
            Ok(None) => self.container.availability(target),
            Ok(Some(message)) => Availability::Unavailable(format!(
                "a run's relay to the queue broker could not be kept running ({message})"
            )),
            Err(error) => Availability::Unavailable(format!(
                "the records of the runs' relays could not be read: {error:#}"
            )),
        }
    }

    fn memory_wait(&self) -> Option<String> {
        self.container.memory_wait()
    }
}

/// The run `run` as `queue` knows it: found among the runs whose task is
/// in progress, else found alone, else missing; unknown when it cannot be
/// read (why is logged).
pub fn relay_run_in(queue: &(impl super::RunReads + ?Sized), run: &str) -> RelayRun {
    let read = (|| {
        if let Some(found) = queue
            .latest_runs_in_progress()?
            .into_iter()
            .find(|found| found.id().as_str() == run)
        {
            return Ok(RelayRun::Found {
                status: found.status(),
                task_in_progress: true,
            });
        }
        let Ok(id) = crate::domain::RunId::new(run) else {
            return Ok(RelayRun::Missing);
        };
        match queue.run(&id) {
            Ok(found) => Ok(RelayRun::Found {
                status: found.status(),
                task_in_progress: false,
            }),
            Err(error) if error.downcast_ref::<super::RunNotFound>().is_some() => {
                Ok(RelayRun::Missing)
            }
            Err(error) => Err(error),
        }
    })();
    read.unwrap_or_else(|error: anyhow::Error| {
        tracing::warn!(run, error = %format_args!("{error:#}"), "the run of a user of the podman machine could not be read: {error:#}");
        RelayRun::Unknown
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::Duration;

    use anyhow::anyhow;

    use super::*;
    use crate::application::ContainerRuntime;
    use crate::application::queue_broker::BrokerProbe;

    struct Records(Result<Option<String>, String>);

    impl RelayRecords for Records {
        fn down(&self) -> Result<Option<String>> {
            self.0.clone().map_err(|error| anyhow!(error))
        }
    }

    struct Usable;

    impl ContainerRuntime for Usable {
        fn availability(&self, _: RuntimeTarget) -> Availability {
            Availability::Available
        }
    }

    #[test]
    fn the_gate_answers_why_while_the_relays_are_down_and_else_asks_on() {
        let gate = |records: Records| RelayGate {
            records: Arc::new(records),
            container: Usable,
        };
        assert_eq!(
            gate(Records(Ok(None))).availability(RuntimeTarget::Worker),
            Availability::Available
        );
        let Availability::Unavailable(why) =
            gate(Records(Ok(Some("start_failed".into())))).availability(RuntimeTarget::Worker)
        else {
            panic!("available while the relays are down");
        };
        assert!(
            why.contains("relay") && why.contains("start_failed"),
            "{why}"
        );
        // A record that cannot be read is fail closed.
        assert!(matches!(
            gate(Records(Err("locked".into()))).availability(RuntimeTarget::Verification),
            Availability::Unavailable(_)
        ));
    }

    #[derive(Default)]
    struct Relays {
        opened: Mutex<Vec<(String, u16)>>,
    }

    impl RelayControl for Relays {
        fn open(&self, spec: &RelaySpec, port: u16) -> Result<()> {
            self.opened.lock().unwrap().push((spec.run.clone(), port));
            Ok(())
        }
        fn close(&self, _: &str) -> Result<()> {
            Ok(())
        }
        fn relays(&self) -> Result<Vec<RelaySeen>> {
            Ok(Vec::new())
        }
        fn restart(&self, _: &str, _: u16) -> Result<()> {
            Ok(())
        }
        fn restart_machine(&self) -> Result<()> {
            Ok(())
        }
        fn machine_runs(&self) -> Result<bool> {
            Ok(true)
        }
        fn stop_unused(&self) -> Result<bool> {
            Ok(false)
        }
    }

    struct Broker(Option<&'static str>);

    impl QueueBrokerControl for Broker {
        fn probe(&self) -> BrokerProbe {
            BrokerProbe {
                state: if self.0.is_some() {
                    ServiceState::Running
                } else {
                    ServiceState::Stopped
                },
                address: self.0.map(str::to_owned),
                pid: None,
                build: None,
                build_matches: None,
                started_at: None,
                error: None,
            }
        }
        fn start(&self, _: Duration) -> Result<BrokerProbe> {
            unreachable!()
        }
        fn stop(&self, _: Duration) -> Result<Option<u32>> {
            unreachable!()
        }
    }

    #[test]
    fn a_relay_opens_towards_the_running_broker_s_port_and_not_without_it() {
        let spec = RelaySpec {
            run: "r1".to_owned(),
            image: "i".to_owned(),
            uid: crate::domain::broker_relay::CONTAINER_UID,
        };
        let relays = Relays::default();
        open(&relays, &Broker(Some("127.0.0.1:4321")), &spec).unwrap();
        assert_eq!(*relays.opened.lock().unwrap(), [("r1".to_owned(), 4321)]);
        let error = open(&relays, &Broker(None), &spec).unwrap_err();
        assert!(format!("{error:#}").contains("not running"), "{error:#}");
        assert_eq!(relays.opened.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_turn_opens_its_relay_again_only_when_the_one_recorded_does_not_serve_it() {
        use crate::domain::broker_relay::RelayState;
        let spec = RelaySpec {
            run: "r1".to_owned(),
            image: "i".to_owned(),
            uid: crate::domain::broker_relay::CONTAINER_UID,
        };
        let seen = |state, target_port| RelaySeen {
            run: "r1".to_owned(),
            state,
            target_port,
            reaches: None,
        };
        let broker = Broker(Some("127.0.0.1:5555"));
        let relays = Relays::default();
        let running_there = seen(RelayState::Running, Some(5555));
        ready_for_turn(&relays, &broker, &spec, Some(&running_there)).unwrap();
        assert!(relays.opened.lock().unwrap().is_empty());
        // Towards the port the broker left while the run was idle, gone
        // with the machine, or none recorded.
        let left = seen(RelayState::Running, Some(4321));
        let gone = seen(RelayState::Stopped, None);
        for seen in [Some(&left), Some(&gone), None] {
            ready_for_turn(&relays, &broker, &spec, seen).unwrap();
        }
        assert_eq!(
            *relays.opened.lock().unwrap(),
            vec![("r1".to_owned(), 5555); 3]
        );
        // A broker that does not run: fail closed, even for a relay that
        // runs.
        let error =
            ready_for_turn(&relays, &Broker(None), &spec, Some(&running_there)).unwrap_err();
        assert!(format!("{error:#}").contains("not running"), "{error:#}");
    }
}
