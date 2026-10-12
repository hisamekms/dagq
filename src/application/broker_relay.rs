//! The relays of the runs' containers ([Container runtime], ADR-t2114-3
//! decisions 2 and 4): how the control side opens a run's socket and relay
//! in the dagq machine before the run's container starts and closes them
//! at the run's end ([`open`], [`close`], through [`RelayControl`]), and
//! how the container runtime cannot be used while the supervisor could not
//! keep the relays running ([`RelayGate`]). The supervisor's watch is
//! [`super::supervise`]'s.
//!
//! What is decided is [`crate::domain::broker_relay`]'s; the podman
//! commands are the adapters'.
//!
//! [Container runtime]: ../../docs/design/container-runtime.md

use std::sync::Arc;

use anyhow::{Context, Result, ensure};

use super::EventStore;
use super::queue_broker::QueueBrokerControl;
use crate::domain::broker_relay::{
    QUEUE_RELAY_ATTENTION_KINDS, RelaySeen, RelaySpec, address_port, attention_stands,
};
use crate::domain::container_runtime::{Availability, RuntimeTarget};
use crate::domain::queue_service::ServiceState;

/// The relays of one queue in the dagq machine, each named by its run.
pub trait RelayControl: Send + Sync {
    /// Make the run's socket volume and start its relay towards the
    /// broker's `port`, and remember it among the queue's relays.
    fn open(&self, spec: &RelaySpec, port: u16) -> Result<()>;
    /// Stop the run's relay, remove its socket volume and forget it; a
    /// relay that is not there is no error.
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
}

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
        .filter(|event| attention_stands(Some(event.kind.as_str())))
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
    let found = broker.probe();
    ensure!(
        found.state == ServiceState::Running,
        "the queue broker is not running ({})",
        found.describe()
    );
    let port = found
        .address
        .as_deref()
        .and_then(address_port)
        .context("the queue broker recorded no port")?;
    control
        .open(spec, port)
        .with_context(|| format!("open the relay of run {}", spec.run))
}

/// At the run's end: close its relay and remove its socket.
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
}
