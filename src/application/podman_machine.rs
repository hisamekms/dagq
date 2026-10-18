//! The dagq podman machine's use cases (ADR-t2113-1 decisions 4 and 5,
//! ADR-t2173-1): make it run before a container starts and before a test
//! or a smoke that needs podman ([`ensure`]), stop it when its last user
//! leaves ([`release`]), both under one host-wide lock; the named volumes
//! inside it ([`MachineVolumes`]); a run's parts in the machine, made
//! ready before each turn of a worker in the container ([`RunMachine`],
//! the one entry this context lends the worker's executor); and how the
//! container runtime cannot be used while the machine cannot
//! ([`MachineGate`]).
//!
//! What is decided is [`crate::domain::podman_machine`]'s; the commands
//! and the lock are the adapters' ([`PodmanMachine`], [`MachineUsers`]).

use std::any::Any;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;

use crate::domain::container_runtime::{Availability, RuntimeTarget};
use crate::domain::podman_machine::{
    LeaseOwner, MACHINE_NAME, MachineEntry, MachineMounts, MachineResources, Prepare,
    mounts_mismatch, prepare, stops,
};

/// The podman commands on the dagq machine, each naming it: never the
/// person's default machine or connection.
pub trait PodmanMachine {
    /// Every machine of the host with whether it runs.
    fn machines(&self) -> Result<Vec<MachineEntry>>;
    /// The host directories the dagq machine was created with as volumes.
    fn mounts(&self) -> Result<Vec<PathBuf>>;
    /// Create the dagq machine with `resources`, mounting each of
    /// `volumes` at the same path; nothing else (not `$HOME`).
    fn init(&self, resources: &MachineResources, volumes: &[PathBuf]) -> Result<()>;
    fn start(&self) -> Result<()>;
    fn stop(&self) -> Result<()>;
}

/// The host-wide lock the machine's start and stop are decided under, and
/// who uses the machine now.
pub trait MachineUsers {
    /// Wait for the host-wide lock; it is held until the value drops.
    fn lock(&self) -> Result<Box<dyn Any>>;
    /// `user` uses the machine until it leaves or `owner` is over
    /// ([`LeaseOwner`]).
    fn join(&self, user: &str, owner: &LeaseOwner) -> Result<()>;
    fn leave(&self, user: &str) -> Result<()>;
    /// The users left whose owner still lives ([`lease_held`]); the others
    /// are forgotten.
    ///
    /// [`lease_held`]: crate::domain::podman_machine::lease_held
    fn count(&self) -> Result<usize>;
}

/// The named volumes of one queue inside the dagq machine (ADR-t2173-1
/// decisions 2 and 3): a run's (or a verification's) build output, and
/// the queue's worker cache. Their names carry the queue, so no call
/// reaches another queue's volume. The machine must run: the run's end
/// and the landing remove a run's build output before its user leaves.
pub trait MachineVolumes {
    /// Create the build output volume of `run` unless it is there.
    fn create_build_output(&self, run: &str) -> Result<()>;
    /// Remove the build output volume of `run`, if it is there.
    fn remove_build_output(&self, run: &str) -> Result<()>;
    fn build_output_exists(&self, run: &str) -> Result<bool>;
    /// Create the queue's worker cache volume unless it is there.
    fn create_worker_cache(&self) -> Result<()>;
    fn remove_worker_cache(&self) -> Result<()>;
    fn worker_cache_exists(&self) -> Result<bool>;
}

/// A run's parts in the dagq machine, made ready before each turn of a
/// worker in the container and before each verification of `integrate` in
/// the container (ADR-t2113-3 decision 1): the machine running with the
/// run among its users ([`ensure`]), the queue's image built
/// ([`super::container_image::prepare`]), the run's relay open
/// ([`super::broker_relay::open`]) and, when asked, a build output volume
/// ([`MachineVolumes`]): the run's for a worker, the verification's own
/// for a verification (ADR-t2173-1 decision 4). This context does it; the
/// executors of the worker and of the verification are given it as this
/// port.
pub trait RunMachine: Send + Sync {
    /// Make the parts of the run `run` ready, and the build output volume
    /// named after `build_output` (the run's ID, or a verification's) when
    /// it is given; the image its container runs. `Err` says why the turn
    /// or the verification cannot run in the container (it runs nowhere
    /// else).
    fn ready(&self, run: &str, build_output: Option<&str>) -> Result<String>;
    /// Remove the build output volume named after `build_output`, if it is
    /// there: a verification's, at its end.
    fn remove_build_output(&self, build_output: &str) -> Result<()>;
}

/// What [`ensure`] and [`release`] act with.
pub struct Machine<'a> {
    pub podman: &'a dyn PodmanMachine,
    pub users: &'a dyn MachineUsers,
    /// The host's resources a new machine is created with.
    pub resources: MachineResources,
    /// The directories of the queue asking, which the machine must mount.
    pub required: MachineMounts,
    /// What a new machine mounts: the directories of every queue of the
    /// host, `required`'s among them.
    pub volumes: Vec<PathBuf>,
}

/// Make the dagq machine run for `user` and count it among the users while
/// `owner` lives: create it if there is none, start it if it is stopped, under the
/// host-wide lock so concurrent runs, verifications and tests do it once.
/// `Err` says why it cannot be used (the person's machine runs, its
/// volumes do not match, a command failed); then `user` is not counted.
/// What was done is returned.
pub fn ensure(machine: &Machine<'_>, user: &str, owner: &LeaseOwner) -> Result<Prepare, String> {
    let _held = machine
        .users
        .lock()
        .map_err(|error| format!("lock the podman machine {MACHINE_NAME}: {error:#}"))?;
    let machines = machine
        .podman
        .machines()
        .map_err(|error| format!("list the podman machines: {error:#}"))?;
    let step = prepare(&machines)?;
    match step {
        Prepare::Init => {
            machine
                .podman
                .init(&machine.resources, &machine.volumes)
                .map_err(|error| format!("create the podman machine {MACHINE_NAME}: {error:#}"))?;
            check_mounts(machine.podman, &machine.required)?;
            start(machine.podman)?;
        }
        Prepare::Start => {
            check_mounts(machine.podman, &machine.required)?;
            start(machine.podman)?;
        }
        Prepare::Ready => check_mounts(machine.podman, &machine.required)?,
    }
    machine
        .users
        .join(user, owner)
        .map_err(|error| format!("count a user of the podman machine {MACHINE_NAME}: {error:#}"))?;
    Ok(step)
}

/// `user` leaves the dagq machine; under the same lock, the machine stops
/// when it runs and no user is left. Whether it was stopped is returned.
pub fn release(machine: &Machine<'_>, user: &str) -> Result<bool> {
    let _held = machine.users.lock()?;
    machine.users.leave(user)?;
    let running = machine
        .podman
        .machines()?
        .iter()
        .any(|entry| entry.name == MACHINE_NAME && entry.running);
    if !stops(running, machine.users.count()?) {
        return Ok(false);
    }
    machine.podman.stop()?;
    Ok(true)
}

/// Stop the dagq machine and start it again, under the host-wide lock:
/// what mends its gvproxy when a relay in it runs and does not reach the
/// host (ADR-t2114-3 decision 4). The users stay counted; a machine that
/// does not run is only started.
pub fn restart(machine: &Machine<'_>) -> Result<()> {
    let _held = machine.users.lock()?;
    let running = machine
        .podman
        .machines()?
        .iter()
        .any(|entry| entry.name == MACHINE_NAME && entry.running);
    if running {
        machine.podman.stop()?;
    }
    machine.podman.start()
}

fn start(podman: &dyn PodmanMachine) -> Result<(), String> {
    podman
        .start()
        .map_err(|error| format!("start the podman machine {MACHINE_NAME}: {error:#}"))
}

fn check_mounts(podman: &dyn PodmanMachine, required: &MachineMounts) -> Result<(), String> {
    let mounted = podman.mounts().map_err(|error| {
        format!("read the volumes of the podman machine {MACHINE_NAME}: {error:#}")
    })?;
    match mounts_mismatch(required, &mounted) {
        Some(why) => Err(why),
        None => Ok(()),
    }
}

/// Why the dagq machine cannot be used by the queue whose directories are
/// `required`, from a look that changes nothing (no lock, no init or
/// start): podman fails, the person's machine runs, or the volumes do not
/// match. A machine that is missing or stopped can be used: [`ensure`]
/// makes it run before a container starts.
pub fn look(podman: &dyn PodmanMachine, required: &MachineMounts) -> Option<String> {
    let machines = match podman.machines() {
        Ok(machines) => machines,
        Err(error) => return Some(format!("list the podman machines: {error:#}")),
    };
    match prepare(&machines) {
        Err(why) => Some(why),
        Ok(Prepare::Init) => None,
        Ok(Prepare::Start | Prepare::Ready) => check_mounts(podman, required).err(),
    }
}

/// The container runtime as it can be used with the dagq machine: while
/// the machine cannot ([`look`]), no target set to the container can run,
/// and the answer says why; else `container` answers.
pub struct MachineGate<C> {
    pub podman: Arc<dyn PodmanMachine + Send + Sync>,
    pub required: MachineMounts,
    pub container: C,
}

impl<C: super::ContainerRuntime> super::ContainerRuntime for MachineGate<C> {
    fn availability(&self, target: RuntimeTarget) -> Availability {
        match look(&*self.podman, &self.required) {
            Some(why) => Availability::Unavailable(why),
            None => self.container.availability(target),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Mutex;

    use anyhow::anyhow;

    use super::*;
    use crate::application::ContainerRuntime;

    /// The machines of a host, what was run on them, and the users.
    #[derive(Default)]
    struct Host {
        machines: Mutex<Vec<MachineEntry>>,
        mounts: Mutex<Vec<PathBuf>>,
        calls: Mutex<Vec<String>>,
        users: Mutex<Vec<String>>,
        fail_start: bool,
    }

    impl Host {
        fn with(machines: &[(&str, bool)]) -> Self {
            Self {
                machines: Mutex::new(
                    machines
                        .iter()
                        .map(|(name, running)| MachineEntry {
                            name: (*name).to_owned(),
                            running: *running,
                            starting: false,
                        })
                        .collect(),
                ),
                ..Self::default()
            }
        }

        fn set_running(&self, running: bool) {
            for machine in self.machines.lock().unwrap().iter_mut() {
                if machine.name == MACHINE_NAME {
                    machine.running = running;
                }
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl PodmanMachine for Host {
        fn machines(&self) -> Result<Vec<MachineEntry>> {
            Ok(self.machines.lock().unwrap().clone())
        }
        fn mounts(&self) -> Result<Vec<PathBuf>> {
            Ok(self.mounts.lock().unwrap().clone())
        }
        fn init(&self, _: &MachineResources, volumes: &[PathBuf]) -> Result<()> {
            self.calls.lock().unwrap().push("init".into());
            self.machines.lock().unwrap().push(MachineEntry {
                name: MACHINE_NAME.into(),
                running: false,
                starting: false,
            });
            *self.mounts.lock().unwrap() = volumes.to_vec();
            Ok(())
        }
        fn start(&self) -> Result<()> {
            self.calls.lock().unwrap().push("start".into());
            if self.fail_start {
                return Err(anyhow!("boom"));
            }
            self.set_running(true);
            Ok(())
        }
        fn stop(&self) -> Result<()> {
            self.calls.lock().unwrap().push("stop".into());
            self.set_running(false);
            Ok(())
        }
    }

    impl MachineUsers for Host {
        fn lock(&self) -> Result<Box<dyn Any>> {
            self.calls.lock().unwrap().push("lock".into());
            Ok(Box::new(()))
        }
        fn join(&self, user: &str, _: &LeaseOwner) -> Result<()> {
            self.users.lock().unwrap().push(user.into());
            Ok(())
        }
        fn leave(&self, user: &str) -> Result<()> {
            self.users.lock().unwrap().retain(|each| each != user);
            Ok(())
        }
        fn count(&self) -> Result<usize> {
            Ok(self.users.lock().unwrap().len())
        }
    }

    const OWNER: LeaseOwner = LeaseOwner::Process { pid: 1 };

    fn machine(host: &Host) -> Machine<'_> {
        let required = MachineMounts::of_queue(Path::new("/q/a"));
        Machine {
            podman: host,
            users: host,
            resources: MachineResources::default(),
            volumes: required.dirs().map(Path::to_path_buf).to_vec(),
            required,
        }
    }

    #[test]
    fn a_missing_machine_is_created_and_started_and_a_stopped_one_started() {
        let host = Host::default();
        assert_eq!(ensure(&machine(&host), "r1", &OWNER), Ok(Prepare::Init));
        assert_eq!(host.calls(), ["lock", "init", "start"]);
        // Running now: nothing more but the lock.
        assert_eq!(ensure(&machine(&host), "r2", &OWNER), Ok(Prepare::Ready));
        assert_eq!(host.calls()[3..], ["lock"]);
        assert_eq!(*host.users.lock().unwrap(), ["r1", "r2"]);

        host.set_running(false);
        assert_eq!(ensure(&machine(&host), "r3", &OWNER), Ok(Prepare::Start));
        assert_eq!(host.calls()[4..], ["lock", "start"]);
    }

    #[test]
    fn a_restart_stops_and_starts_a_running_machine_under_the_lock_and_keeps_its_users() {
        let host = Host::with(&[(MACHINE_NAME, true)]);
        host.users.lock().unwrap().push("r1".into());
        restart(&machine(&host)).unwrap();
        assert_eq!(host.calls(), ["lock", "stop", "start"]);
        assert_eq!(*host.users.lock().unwrap(), ["r1"]);
        // A stopped machine is only started.
        host.set_running(false);
        restart(&machine(&host)).unwrap();
        assert_eq!(host.calls()[3..], ["lock", "start"]);
    }

    #[test]
    fn the_machine_stops_when_its_last_user_leaves_under_the_lock() {
        let host = Host::default();
        ensure(&machine(&host), "r1", &OWNER).unwrap();
        ensure(&machine(&host), "r2", &OWNER).unwrap();
        assert!(!release(&machine(&host), "r1").unwrap());
        assert!(release(&machine(&host), "r2").unwrap());
        assert_eq!(host.calls()[4..], ["lock", "lock", "stop"]);
        // Stopped already: nothing to stop.
        assert!(!release(&machine(&host), "r2").unwrap());
    }

    #[test]
    fn while_the_person_s_machine_runs_nothing_is_created_started_or_counted() {
        let host = Host::with(&[("podman-machine-default", true)]);
        let why = ensure(&machine(&host), "r1", &OWNER).unwrap_err();
        assert!(why.contains("podman-machine-default runs"), "{why}");
        assert_eq!(host.calls(), ["lock"]);
        assert!(host.users.lock().unwrap().is_empty());
    }

    #[test]
    fn volumes_that_do_not_match_are_not_started_and_say_how_to_recreate() {
        let host = Host::with(&[(MACHINE_NAME, false)]);
        *host.mounts.lock().unwrap() = vec![PathBuf::from("/Users/me")];
        let why = ensure(&machine(&host), "r1", &OWNER).unwrap_err();
        assert!(why.contains("podman machine rm dagq"), "{why}");
        assert_eq!(host.calls(), ["lock"]);
        assert!(host.users.lock().unwrap().is_empty());
        assert!(look(&host, &machine(&host).required).is_some());
    }

    #[test]
    fn a_failed_start_is_unavailable_and_not_counted() {
        let host = Host {
            fail_start: true,
            ..Host::with(&[(MACHINE_NAME, false)])
        };
        *host.mounts.lock().unwrap() = machine(&host).volumes.clone();
        let why = ensure(&machine(&host), "r1", &OWNER).unwrap_err();
        assert!(why.contains("start the podman machine dagq: boom"), "{why}");
        assert!(host.users.lock().unwrap().is_empty());
    }

    struct Usable;

    impl ContainerRuntime for Usable {
        fn availability(&self, _: RuntimeTarget) -> Availability {
            Availability::Available
        }
    }

    #[test]
    fn the_gate_answers_why_the_machine_cannot_be_used_and_else_asks_on() {
        let gate = |host: Host| MachineGate {
            podman: Arc::new(host),
            required: MachineMounts::of_queue(Path::new("/q/a")),
            container: Usable,
        };
        // Missing: ensure creates it before a container starts.
        assert_eq!(
            gate(Host::default()).availability(RuntimeTarget::Worker),
            Availability::Available
        );
        let Availability::Unavailable(why) = gate(Host::with(&[("podman-machine-default", true)]))
            .availability(RuntimeTarget::Verification)
        else {
            panic!("usable while the person's machine runs");
        };
        assert!(why.contains("cannot start"), "{why}");
        // Created with $HOME: unusable, with how to recreate it.
        let home = Host::with(&[(MACHINE_NAME, true)]);
        *home.mounts.lock().unwrap() = vec![PathBuf::from("/Users/me")];
        assert!(matches!(
            gate(home).availability(RuntimeTarget::Worker),
            Availability::Unavailable(why) if why.contains("podman machine rm")
        ));
    }
}
