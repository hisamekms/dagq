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
//! Before a turn's or a verification's container starts, it reserves its
//! memory limit of the machine under the same lock, and waits outside it
//! while it does not fit ([`try_reserve`], [`wait_for_memory`],
//! [`release_memory`], ADR-t3163-1); a new claim looks first
//! ([`look_room`]).
//!
//! What is decided is [`crate::domain::podman_machine`]'s and
//! [`crate::domain::machine_memory`]'s; the commands, the lock and the
//! records are the adapters' ([`PodmanMachine`], [`MachineUsers`],
//! [`MachineMemory`], [`MachineReservations`]).

use std::any::Any;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;

use crate::domain::container_runtime::{Availability, RuntimeTarget};
use crate::domain::machine_memory::{
    Judgement, MachineContainer, MemoryAsk, Tried, WaitMove, WaitSoFar, claim_wait, judge,
    next_seq, refusal, reservation_held, wait_held, wait_next,
};
pub use crate::domain::machine_memory::{MemoryNote, MemoryStep};
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
    /// The host-wide lock when it is free now, `None` when another holds
    /// it (a machine starting): for what may be left to a later holder.
    fn try_lock(&self) -> Result<Option<Box<dyn Any>>> {
        self.lock().map(Some)
    }
    /// `user` uses the machine until it leaves or `owner` is over
    /// ([`LeaseOwner`]).
    fn join(&self, user: &str, owner: &LeaseOwner) -> Result<()>;
    fn leave(&self, user: &str) -> Result<()>;
    /// Whether `user` is among the users now, whether its owner lives or
    /// not.
    fn joined(&self, user: &str) -> Result<bool>;
    /// The users left whose owner still lives ([`lease_held`]); the others
    /// are forgotten.
    ///
    /// [`lease_held`]: crate::domain::podman_machine::lease_held
    fn count(&self) -> Result<usize>;
}

/// What the dagq machine holds, read from inside it (ADR-t3163-1 decision
/// 1): the machine must run for its containers to be read.
pub trait MachineMemory {
    /// The memory the dagq machine has, in MiB: what it was created with,
    /// which `host.toml` changes only for a machine created again.
    fn memory_mib(&self) -> Result<u64>;
    /// The dagq containers it runs (those labelled with a queue), each
    /// with its memory limit and whether it is a relay.
    fn containers(&self) -> Result<Vec<MachineContainer>>;
}

/// The reservations and the waits of the dagq machine's memory, of every
/// queue of the host, read and written under [`MachineUsers::lock`]
/// (ADR-t3163-1 decision 3).
pub trait MachineReservations {
    fn reservations(&self) -> Result<Vec<MemoryAsk>>;
    fn waits(&self) -> Result<Vec<MemoryAsk>>;
    /// Record `ask` as reserved, in place of its wait.
    fn reserve(&self, ask: &MemoryAsk) -> Result<()>;
    /// Forget the reservation of `container`, if any.
    fn unreserve(&self, container: &str) -> Result<()>;
    /// Record `ask` as waiting, unless its wait is there.
    fn wait(&self, ask: &MemoryAsk) -> Result<()>;
    /// Forget the wait of `container`, if any.
    fn unwait(&self, container: &str) -> Result<()>;
    /// Whether the run or process `owner` still uses the machine
    /// ([`crate::domain::podman_machine::lease_held`]).
    fn owner_held(&self, owner: &LeaseOwner) -> bool;
    fn process_alive(&self, pid: u32) -> bool;
}

/// What a container's memory is reserved with: the machine's lock, what
/// is in it and the records.
pub struct MemoryMachine<'a> {
    pub users: &'a dyn MachineUsers,
    pub memory: &'a dyn MachineMemory,
    pub records: &'a dyn MachineReservations,
}

/// The reservations and the waits that still hold, the machine running
/// `containers` at `now`; the others are forgotten when `forget`.
fn standing(
    records: &dyn MachineReservations,
    containers: &[MachineContainer],
    now: i64,
    forget: bool,
) -> Result<(Vec<MemoryAsk>, Vec<MemoryAsk>)> {
    let mut held = Vec::new();
    for reservation in records.reservations()? {
        let on_machine = containers
            .iter()
            .any(|container| container.name == reservation.container);
        if reservation_held(
            &reservation,
            records.owner_held(&reservation.owner),
            records.process_alive(reservation.pid),
            on_machine,
            now,
        ) {
            held.push(reservation);
        } else if forget {
            records.unreserve(&reservation.container)?;
        }
    }
    let mut waiting = Vec::new();
    for wait in records.waits()? {
        if wait_held(
            records.owner_held(&wait.owner),
            records.process_alive(wait.pid),
        ) {
            waiting.push(wait);
        } else if forget {
            records.unwait(&wait.container)?;
        }
    }
    Ok((held, waiting))
}

/// One try at `ask`'s memory at `now`, under the machine's host-wide lock
/// (ADR-t3163-1 decisions 1 and 3): the reservations whose container or
/// owner is over and the waits whose process or owner is are forgotten
/// first, then [`judge`] decides. Reserved, the ask is recorded as taken
/// at `now` in place of its wait; waiting, its wait is recorded once, at
/// when it began (`ask.since`), for the host-wide order; refused, its wait
/// goes. A machine whose containers or memory cannot be read is not taken
/// for empty: the ask waits. `Err` only when the lock or the records
/// fail.
pub fn try_reserve(machine: &MemoryMachine<'_>, ask: &MemoryAsk, now: i64) -> Result<MemoryStep> {
    let _held = machine.users.lock()?;
    // Its place in the host-wide order, kept from its first wait.
    let ask = &MemoryAsk {
        seq: next_seq(&machine.records.waits()?, &ask.container),
        ..ask.clone()
    };
    let read = machine
        .memory
        .containers()
        .and_then(|containers| Ok((machine.memory.memory_mib()?, containers)));
    let (memory_mib, containers) = match read {
        Ok(read) => read,
        Err(error) => {
            machine.records.wait(ask)?;
            return Ok(MemoryStep::Wait(format!(
                "what the dagq machine {MACHINE_NAME} runs could not be read ({error:#}): no \
container starts until it is"
            )));
        }
    };
    let (reservations, waits) = standing(machine.records, &containers, now, true)?;
    match judge(memory_mib, &containers, &reservations, &waits, ask) {
        Judgement::Reserve => {
            machine.records.reserve(&MemoryAsk {
                since: now,
                started: None,
                ..ask.clone()
            })?;
            Ok(MemoryStep::Reserved)
        }
        Judgement::Wait(why) => {
            machine.records.wait(ask)?;
            Ok(MemoryStep::Wait(why))
        }
        Judgement::Refuse(why) => {
            machine.records.unwait(&ask.container)?;
            Ok(MemoryStep::Refused(why))
        }
    }
}

/// Record under the machine's lock that the container of the turn
/// reserved as `container` was started at `now`: from then on its
/// reservation ends with the container ([`reservation_held`]). Nothing
/// when it has no reservation.
pub fn mark_started(machine: &MemoryMachine<'_>, container: &str, now: i64) -> Result<()> {
    let _held = machine.users.lock()?;
    let Some(held) = machine
        .records
        .reservations()?
        .into_iter()
        .find(|held| held.container == container)
    else {
        return Ok(());
    };
    machine.records.reserve(&MemoryAsk {
        started: Some(now),
        ..held
    })
}

/// Forget the reservation and the wait of `container` under the machine's
/// lock: a verification's at its cleanup, whose process lives on and
/// would keep it. The others over are forgotten at the next try.
pub fn release_memory(machine: &MemoryMachine<'_>, container: &str) -> Result<()> {
    let _held = machine.users.lock()?;
    machine.records.unreserve(container)?;
    machine.records.unwait(container)
}

/// [`release_memory`] when the lock is free now, without waiting for it
/// (a stop of a run on the supervisor's pass is not held by a machine that
/// starts): a turn's reservation left is forgotten at the next try, as its
/// container is gone. Whether it was forgotten now is returned.
pub fn release_memory_if_free(machine: &MemoryMachine<'_>, container: &str) -> Result<bool> {
    let Some(_held) = machine.users.try_lock()? else {
        return Ok(false);
    };
    machine.records.unreserve(container)?;
    machine.records.unwait(container)?;
    Ok(true)
}

/// What a look at the machine's memory for a new claim found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoomLook {
    /// A worker's container fits.
    Fits,
    /// It would wait, for why: the claim waits instead.
    Wait(String),
    /// It never fits: the container cannot be used.
    Refused(String),
}

/// Whether a worker's container of `limit_mib` would fit now, from a look
/// that takes no lock and changes nothing (the supervisor's pass is not
/// held by a machine that starts): a machine that does not run has no
/// container, and one not created yet has the memory of `resources`. A
/// machine that cannot be read is no room.
pub fn look_room(
    podman: &dyn PodmanMachine,
    memory: &dyn MachineMemory,
    records: &dyn MachineReservations,
    resources: &MachineResources,
    limit_mib: u64,
    now: i64,
) -> RoomLook {
    let read = || -> Result<(u64, Vec<MachineContainer>)> {
        let machines = podman.machines()?;
        Ok(
            match machines.iter().find(|machine| machine.name == MACHINE_NAME) {
                None => (resources.memory_mib, Vec::new()),
                Some(machine) if !machine.running => (memory.memory_mib()?, Vec::new()),
                Some(_) => (memory.memory_mib()?, memory.containers()?),
            },
        )
    };
    let looked = read().and_then(|(memory_mib, containers)| {
        let (reservations, waits) = standing(records, &containers, now, false)?;
        Ok((memory_mib, containers, reservations, waits))
    });
    let (memory_mib, containers, reservations, waits) = match looked {
        Ok(looked) => looked,
        Err(error) => {
            return RoomLook::Wait(format!(
                "the memory of the dagq machine {MACHINE_NAME} could not be read: {error:#}"
            ));
        }
    };
    let relays = containers
        .iter()
        .filter(|container| container.relay)
        .count();
    if let Some(why) = refusal(memory_mib, relays, limit_mib) {
        return RoomLook::Refused(why);
    }
    match claim_wait(memory_mib, &containers, &reservations, &waits, limit_mib) {
        Some(why) => RoomLook::Wait(why),
        None => RoomLook::Fits,
    }
}

/// Why a wait for the machine's memory ended before its container
/// started: what waits is to end ([`WaitMove::Stop`]). What it reserved or
/// waited for is to be forgotten, and its container does not start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryWaitStopped(pub String);

impl std::fmt::Display for MemoryWaitStopped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the wait for the dagq machine's memory ended: {}",
            self.0
        )
    }
}

impl std::error::Error for MemoryWaitStopped {}

/// Try an ask with `try_once` (at the Unix second of the millisecond
/// `now_ms` reads) until it is reserved, pausing with `pause` between the
/// tries: outside the machine's lock, so neither `ensure` nor the relays'
/// watch waits for it (ADR-t3163-1 decision 4). After each try `stop` is
/// read, and [`wait_next`] decides what is told to `notes` and what is
/// done. `Err` says why the container cannot start: its limit never fits,
/// a try failed, or what waits is to end ([`MemoryWaitStopped`]).
pub fn wait_for_memory(
    mut try_once: impl FnMut(i64) -> Result<MemoryStep>,
    now_ms: &dyn Fn() -> i64,
    pause: &dyn Fn(),
    stop: &dyn Fn() -> Option<String>,
    notes: &mut dyn FnMut(MemoryNote<'_>),
) -> Result<()> {
    let mut so_far = WaitSoFar::default();
    loop {
        let at = now_ms();
        let (tried, failure) = match try_once(at.div_euclid(1000)) {
            Ok(MemoryStep::Reserved) => (Tried::Reserved, None),
            Ok(MemoryStep::Wait(why)) => (Tried::Wait(why), None),
            Ok(MemoryStep::Refused(why)) => (Tried::Refused(why), None),
            Err(error) => (Tried::Failed, Some(error)),
        };
        let stopped = stop();
        let (told, then, next) = wait_next(so_far, at, &tried, stopped.as_deref());
        for note in told {
            notes(note);
        }
        so_far = next;
        match then {
            WaitMove::Start => return Ok(()),
            WaitMove::Pause => pause(),
            WaitMove::Refuse(why) => anyhow::bail!("{why}"),
            WaitMove::Fail => {
                return Err(failure.unwrap_or_else(|| anyhow::anyhow!("the try failed")));
            }
            WaitMove::Stop(why) => return Err(MemoryWaitStopped(why).into()),
        }
    }
}

/// How the dagq machine's memory looks for a new claim (the
/// [`MachineGate`]'s, ADR-t3163-1 decision 4).
pub trait MemoryRoom: Send + Sync {
    /// [`look_room`] for a worker's container now.
    fn look(&self) -> RoomLook;
}

/// The named volumes of one queue inside the dagq machine (ADR-t2173-1
/// decisions 2 and 3): a run's (or a verification's) build output, and
/// the queue's worker cache, a volume for each directory of the container
/// it is mounted at. Their names carry the queue, so no call
/// reaches another queue's volume. The machine must run: the run's end
/// and the landing remove a run's build output before its user leaves.
pub trait MachineVolumes {
    /// Create the build output volume of `run` unless it is there.
    fn create_build_output(&self, run: &str) -> Result<()>;
    /// Remove the build output volume of `run`, if it is there.
    fn remove_build_output(&self, run: &str) -> Result<()>;
    fn build_output_exists(&self, run: &str) -> Result<bool>;
    /// Create the queue's worker cache volume of the container's
    /// directory `dir` unless it is there.
    fn create_worker_cache(&self, dir: &str) -> Result<()>;
    fn remove_worker_cache(&self, dir: &str) -> Result<()>;
    fn worker_cache_exists(&self, dir: &str) -> Result<bool>;
}

/// A run's parts in the dagq machine, made ready before each turn of a
/// worker in the container and before each verification of `integrate` in
/// the container (ADR-t2113-3 decision 1): the machine running with the
/// run among its users ([`ensure`]), the queue's image built
/// ([`super::container_image::prepare`]), the run's relay open
/// ([`super::broker_relay::open`]) and, when asked, a build output volume
/// ([`MachineVolumes`]): the run's for a worker, the verification's own
/// for a verification (ADR-t2173-1 decision 4). A worker's turn also has
/// the queue's worker cache volumes made ([`Self::create_worker_cache`]);
/// a verification never mounts them (ADR-t2114-4 decision 4). This context does it; the
/// executors of the worker and of the verification are given it as this
/// port.
pub trait RunMachine: Send + Sync {
    /// Make the parts of the run `run` ready, and the build output volume
    /// named after `build_output` (the run's ID, or a verification's) when
    /// it is given; the image its container runs. `Err` says why the turn
    /// or the verification cannot run in the container (it runs nowhere
    /// else).
    fn ready(&self, run: &str, build_output: Option<&str>) -> Result<String>;
    /// Make the dagq machine run for the run `run` and its image built,
    /// and nothing else: no relay of the run towards the queue broker and
    /// no volume. The image a facts-only turn's container runs.
    fn ready_image(&self, run: &str) -> Result<String>;
    /// Remove the build output volume named after `build_output`, if it is
    /// there: a verification's, at its end.
    fn remove_build_output(&self, build_output: &str) -> Result<()>;
    /// Create the queue's worker cache volume of each of `dirs`, the
    /// container's directories a worker's turn mounts them at, unless it
    /// is there. The machine must run ([`Self::ready`] first).
    fn create_worker_cache(&self, dirs: &[String]) -> Result<()>;
    /// One try at `ask`'s memory at `now` ([`try_reserve`]), after
    /// [`Self::ready`] made the machine run with the run among its users.
    fn reserve(&self, ask: &MemoryAsk, now: i64) -> Result<MemoryStep>;
    /// Forget the reservation and the wait of the container `container`
    /// ([`release_memory`]).
    fn release_memory(&self, container: &str) -> Result<()>;
    /// Record that the turn's container reserved as `container` was
    /// started at `now` ([`mark_started`]).
    fn mark_started(&self, container: &str, now: i64) -> Result<()> {
        let _ = (container, now);
        Ok(())
    }
    /// [`Self::release_memory`] without waiting for the machine's lock
    /// ([`release_memory_if_free`]): what a turn's container's removal
    /// does.
    fn release_memory_if_free(&self, container: &str) -> Result<()> {
        self.release_memory(container)
    }
    /// This machine with everything it does ended by `deadline`: each wait
    /// for the host-wide lock gives up then instead of waiting on, and each
    /// podman command (the machine's start, the image's build, the reads of
    /// its memory) is killed then; either is an `Err`. What a facts-only
    /// turn on the supervisor's pass readies with, so a machine or a lock
    /// that does not come back holds the pass for no longer. `None` when
    /// this machine has no such end (it never waits on a host's machine).
    fn ending_by(&self, deadline: std::time::Instant) -> Option<Arc<dyn RunMachine>> {
        let _ = deadline;
        None
    }
    /// Make the dagq machine run for this process's own work in it for a
    /// run that may be over (the removal of what an earlier verification
    /// left, the facts-only turn of a run that failed or was interrupted),
    /// counted as the user `user` while this process lives or until
    /// [`Self::leave`] ([`ensure`]), and nothing else: no image, relay or
    /// volume. Such a run is not counted, so without it the machine could
    /// be stopped under the work as unused.
    fn hold(&self, user: &str) -> Result<()> {
        let _ = user;
        Ok(())
    }
    /// `user` leaves the dagq machine, which stops when no user is left
    /// ([`release`]). Whether it was stopped is returned.
    fn leave(&self, user: &str) -> Result<bool> {
        let _ = user;
        Ok(false)
    }
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

/// [`ensure`] for `user`, then `then` with the machine running. When
/// `then` fails once its deadline passed (`passed`: its commands were cut
/// off by it)
/// and this made `user` a user (it was none before), `user` leaves again,
/// so a readying cut off by its deadline keeps no lease of the machine. It
/// leaves without the host-wide lock, which may be held past the deadline,
/// and the machine is left running for the next [`release`] to stop. A
/// failure before the deadline keeps the lease, as [`ensure`]'s does.
pub fn ensure_then<T>(
    machine: &Machine<'_>,
    user: &str,
    owner: &LeaseOwner,
    passed: &dyn Fn() -> bool,
    then: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let joined = machine.users.joined(user)?;
    ensure(machine, user, owner)
        .map_err(anyhow::Error::msg)
        .map_err(|error| error.context("make the dagq machine run"))?;
    let done = then();
    if done.is_err()
        && !joined
        && passed()
        && let Err(error) = machine.users.leave(user)
    {
        tracing::warn!(user = %user, error = %format_args!("{error:#}"), "{user} could not leave the dagq machine {MACHINE_NAME} after its readying was cut off: {error:#}");
    }
    done
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

/// Stop the dagq machine when it runs and no user is held, under the
/// host-wide lock, making no user leave: the users whose owner is over are
/// forgotten as they are counted ([`MachineUsers::count`]). What the
/// supervisor's relay watch does once runs ended: a run that joined again
/// meanwhile (resumed under the same ID) is held and keeps the machine,
/// where a leave by its ID would forget it. Whether it was stopped is
/// returned.
pub fn stop_unused(machine: &Machine<'_>) -> Result<bool> {
    let _held = machine.users.lock()?;
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
    match mounts_mismatch(required, required.data_root(), &mounted) {
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
///
/// A container's limit beyond what the machine leaves the containers makes
/// every target unusable too, and while a worker's container would wait
/// for the machine's memory, a new claim waits instead ([`MemoryRoom`],
/// ADR-t3163-1 decisions 3 and 4).
pub struct MachineGate<C> {
    pub podman: Arc<dyn PodmanMachine + Send + Sync>,
    pub required: MachineMounts,
    pub room: Arc<dyn MemoryRoom>,
    pub container: C,
}

impl<C: super::ContainerRuntime> super::ContainerRuntime for MachineGate<C> {
    fn availability(&self, target: RuntimeTarget) -> Availability {
        if let Some(why) = look(&*self.podman, &self.required) {
            return Availability::Unavailable(why);
        }
        match self.room.look() {
            RoomLook::Refused(why) => Availability::Unavailable(why),
            RoomLook::Fits | RoomLook::Wait(_) => self.container.availability(target),
        }
    }

    fn memory_wait(&self) -> Option<String> {
        match self.room.look() {
            RoomLook::Wait(why) => Some(why),
            RoomLook::Fits | RoomLook::Refused(_) => self.container.memory_wait(),
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
        fn joined(&self, user: &str) -> Result<bool> {
            Ok(self.users.lock().unwrap().iter().any(|each| each == user))
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

    /// Acceptance (d) of the facts-only turn's deadline: a readying cut
    /// off by its deadline after the machine started leaves no lease of
    /// it, and takes no lock to leave; one that fails before its deadline
    /// keeps its lease, and so does a user that was one before.
    #[test]
    fn a_readying_cut_off_by_its_deadline_leaves_the_machine_it_joined() {
        let passed = || true;
        let ahead = || false;
        let host = Host::default();
        let cut_off = ensure_then(&machine(&host), "r1", &OWNER, &passed, || {
            Err::<(), _>(anyhow!("the build was killed"))
        });
        assert!(cut_off.is_err());
        assert_eq!(host.count().unwrap(), 0);
        assert_eq!(host.calls(), ["lock", "init", "start"]);
        let failed = ensure_then(&machine(&host), "r1", &OWNER, &ahead, || {
            Err::<(), _>(anyhow!("no image"))
        });
        assert!(failed.is_err());
        assert!(host.joined("r1").unwrap());
        let again = ensure_then(&machine(&host), "r1", &OWNER, &passed, || {
            Err::<(), _>(anyhow!("the build was killed"))
        });
        assert!(again.is_err());
        assert!(host.joined("r1").unwrap());
        assert_eq!(
            ensure_then(&machine(&host), "r2", &OWNER, &passed, || Ok(7)).unwrap(),
            7
        );
        assert!(host.joined("r2").unwrap());
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

    struct Room(RoomLook);

    impl MemoryRoom for Room {
        fn look(&self) -> RoomLook {
            self.0.clone()
        }
    }

    #[test]
    fn the_gate_answers_why_the_machine_cannot_be_used_and_else_asks_on() {
        let gate = |host: Host| MachineGate {
            podman: Arc::new(host),
            required: MachineMounts::of_queue(Path::new("/q/a")),
            room: Arc::new(Room(RoomLook::Fits)),
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

    /// A limit that never fits the machine makes the container unusable;
    /// a worker's container that would wait holds a new claim, with why.
    #[test]
    fn the_gate_refuses_a_limit_that_never_fits_and_tells_a_claim_to_wait() {
        let gate = |look: RoomLook| MachineGate {
            podman: Arc::new(Host::default()),
            required: MachineMounts::of_queue(Path::new("/q/a")),
            room: Arc::new(Room(look)),
            container: Usable,
        };
        let refused = gate(RoomLook::Refused("too large".into()));
        assert_eq!(
            refused.availability(RuntimeTarget::Verification),
            Availability::Unavailable("too large".into())
        );
        assert_eq!(refused.memory_wait(), None);
        let waits = gate(RoomLook::Wait("full".into()));
        assert_eq!(
            waits.availability(RuntimeTarget::Worker),
            Availability::Available
        );
        assert_eq!(waits.memory_wait().as_deref(), Some("full"));
        assert_eq!(gate(RoomLook::Fits).memory_wait(), None);
    }

    use crate::domain::machine_memory::{MemoryAsk, Purpose, START_GRACE_SECS};

    /// A machine's memory, its containers and the records of the
    /// reservations and the waits; owners and processes named in `over`
    /// are over.
    #[derive(Default)]
    struct Memory {
        memory_mib: u64,
        containers: Mutex<Option<Vec<MachineContainer>>>,
        reservations: Mutex<Vec<MemoryAsk>>,
        waits: Mutex<Vec<MemoryAsk>>,
        over: Mutex<Vec<u32>>,
    }

    impl Memory {
        fn of(memory_mib: u64) -> Self {
            Self {
                memory_mib,
                containers: Mutex::new(Some(Vec::new())),
                ..Self::default()
            }
        }

        fn names(asks: &Mutex<Vec<MemoryAsk>>) -> Vec<String> {
            asks.lock()
                .unwrap()
                .iter()
                .map(|ask| ask.container.clone())
                .collect()
        }

        fn run(&self, name: &str, memory_mib: u64) {
            self.containers
                .lock()
                .unwrap()
                .get_or_insert_with(Vec::new)
                .push(MachineContainer {
                    name: name.into(),
                    memory_mib: Some(memory_mib),
                    relay: false,
                });
        }
    }

    impl MachineMemory for Memory {
        fn memory_mib(&self) -> Result<u64> {
            Ok(self.memory_mib)
        }
        fn containers(&self) -> Result<Vec<MachineContainer>> {
            self.containers
                .lock()
                .unwrap()
                .clone()
                .ok_or_else(|| anyhow!("unable to connect to Podman"))
        }
    }

    impl MachineReservations for Memory {
        fn reservations(&self) -> Result<Vec<MemoryAsk>> {
            Ok(self.reservations.lock().unwrap().clone())
        }
        fn waits(&self) -> Result<Vec<MemoryAsk>> {
            Ok(self.waits.lock().unwrap().clone())
        }
        fn reserve(&self, ask: &MemoryAsk) -> Result<()> {
            self.unwait(&ask.container)?;
            self.unreserve(&ask.container)?;
            self.reservations.lock().unwrap().push(ask.clone());
            Ok(())
        }
        fn unreserve(&self, container: &str) -> Result<()> {
            self.reservations
                .lock()
                .unwrap()
                .retain(|ask| ask.container != container);
            Ok(())
        }
        fn wait(&self, ask: &MemoryAsk) -> Result<()> {
            let mut waits = self.waits.lock().unwrap();
            if !waits.iter().any(|wait| wait.container == ask.container) {
                waits.push(ask.clone());
            }
            Ok(())
        }
        fn unwait(&self, container: &str) -> Result<()> {
            self.waits
                .lock()
                .unwrap()
                .retain(|ask| ask.container != container);
            Ok(())
        }
        fn owner_held(&self, owner: &LeaseOwner) -> bool {
            match owner {
                LeaseOwner::Process { pid } => !self.over.lock().unwrap().contains(pid),
                LeaseOwner::Run { .. } => true,
            }
        }
        fn process_alive(&self, pid: u32) -> bool {
            !self.over.lock().unwrap().contains(&pid)
        }
    }

    fn ask(container: &str, purpose: Purpose, memory_mib: u64, pid: u32) -> MemoryAsk {
        MemoryAsk {
            container: container.into(),
            purpose,
            memory_mib,
            owner: LeaseOwner::Process { pid },
            pid,
            since: 0,
            seq: 0,
            started: None,
        }
    }

    /// Under the lock, a container that fits is reserved, one that does
    /// not waits in the host-wide order, a limit that never fits is
    /// refused, and a machine whose containers cannot be read is no room.
    #[test]
    fn a_try_reserves_what_fits_and_records_the_wait_of_what_does_not() {
        let host = Host::default();
        let memory = Memory::of(9216);
        let machine = MemoryMachine {
            users: &host,
            memory: &memory,
            records: &memory,
        };
        let first = ask("dagq-q-run-r1", Purpose::Turn, 4096, 1);
        assert_eq!(
            try_reserve(&machine, &first, 10).unwrap(),
            MemoryStep::Reserved
        );
        assert_eq!(host.calls(), ["lock"]);
        // Taken at the try, for the start's grace.
        assert_eq!(memory.reservations.lock().unwrap()[0].since, 10);
        let second = ask("dagq-q-run-r2", Purpose::Turn, 4096, 2);
        assert_eq!(
            try_reserve(&machine, &second, 11).unwrap(),
            MemoryStep::Reserved
        );
        let third = MemoryAsk {
            since: 12,
            ..ask("dagq-p-run-r3", Purpose::Turn, 4096, 3)
        };
        let MemoryStep::Wait(why) = try_reserve(&machine, &third, 12).unwrap() else {
            panic!("a third fitted");
        };
        assert!(why.contains("does not fit"), "{why}");
        assert_eq!(Memory::names(&memory.waits), ["dagq-p-run-r3"]);
        // Its wait keeps when it began however often it tries.
        try_reserve(&machine, &third, 40).unwrap();
        assert_eq!(memory.waits.lock().unwrap()[0].since, 12);
        // A verification that came later goes before it once room is made.
        let verification = MemoryAsk {
            since: 50,
            ..ask("dagq-q-verify-v1", Purpose::Verification, 4096, 4)
        };
        assert!(matches!(
            try_reserve(&machine, &verification, 50).unwrap(),
            MemoryStep::Wait(_)
        ));
        release_memory(&machine, "dagq-q-run-r1").unwrap();
        assert!(matches!(
            try_reserve(&machine, &third, 61).unwrap(),
            MemoryStep::Wait(why) if why.contains("dagq-q-verify-v1")
        ));
        assert_eq!(
            try_reserve(&machine, &verification, 62).unwrap(),
            MemoryStep::Reserved
        );
        assert_eq!(Memory::names(&memory.waits), ["dagq-p-run-r3"]);
        // Never fits: refused, its wait gone.
        let huge = ask("dagq-p-run-r3", Purpose::Turn, 9000, 3);
        assert!(matches!(
            try_reserve(&machine, &huge, 63).unwrap(),
            MemoryStep::Refused(why) if why.contains("host.toml")
        ));
        assert!(Memory::names(&memory.waits).is_empty());
        // The machine cannot be read: nothing is taken for empty.
        *memory.containers.lock().unwrap() = None;
        let fourth = ask("dagq-q-run-r4", Purpose::Turn, 1, 5);
        assert!(matches!(
            try_reserve(&machine, &fourth, 64).unwrap(),
            MemoryStep::Wait(why) if why.contains("could not be read")
        ));
    }

    /// Each try forgets a turn's reservation whose container started and is
    /// gone, and what an owner or a process that is over left; a
    /// verification's reservation outlives its commands' containers.
    #[test]
    fn a_try_forgets_what_is_over_and_keeps_a_verification_between_its_commands() {
        let host = Host::default();
        let memory = Memory::of(9216);
        let machine = MemoryMachine {
            users: &host,
            memory: &memory,
            records: &memory,
        };
        let turn = ask("dagq-q-run-r1", Purpose::Turn, 4096, 1);
        let verification = ask("dagq-q-verify-v1", Purpose::Verification, 4096, 2);
        try_reserve(&machine, &turn, 0).unwrap();
        try_reserve(&machine, &verification, 0).unwrap();
        mark_started(&machine, "dagq-q-run-r1", 0).unwrap();
        assert!(
            memory
                .reservations
                .lock()
                .unwrap()
                .iter()
                .any(|held| held.container == "dagq-q-run-r1" && held.started == Some(0))
        );
        memory.run("dagq-q-run-r1", 4096);
        let other = ask("dagq-q-run-r2", Purpose::Turn, 4096, 3);
        // Full: the turn's container runs, the verification is between two
        // commands.
        assert!(matches!(
            try_reserve(&machine, &other, START_GRACE_SECS + 1).unwrap(),
            MemoryStep::Wait(_)
        ));
        // The turn's container ended: its reservation goes at the next try.
        memory.containers.lock().unwrap().as_mut().unwrap().clear();
        assert_eq!(
            try_reserve(&machine, &other, START_GRACE_SECS + 2).unwrap(),
            MemoryStep::Reserved
        );
        assert_eq!(
            Memory::names(&memory.reservations),
            ["dagq-q-verify-v1", "dagq-q-run-r2"]
        );
        // The verification's process is gone, and a wait's too: both go at
        // the next try.
        memory.over.lock().unwrap().push(2);
        memory
            .waits
            .lock()
            .unwrap()
            .push(ask("dagq-q-run-r9", Purpose::Turn, 1, 9));
        memory.over.lock().unwrap().push(9);
        let small = ask("dagq-q-run-r3", Purpose::Turn, 1, 4);
        try_reserve(&machine, &small, START_GRACE_SECS + 3).unwrap();
        assert_eq!(
            Memory::names(&memory.reservations),
            ["dagq-q-run-r2", "dagq-q-run-r3"]
        );
        assert!(Memory::names(&memory.waits).is_empty());
        // A release forgets its own at once, and without waiting when the
        // lock is free.
        release_memory(&machine, "dagq-q-run-r2").unwrap();
        assert!(release_memory_if_free(&machine, "dagq-q-run-r3").unwrap());
        assert!(Memory::names(&memory.reservations).is_empty());
    }

    /// The claim's look takes no lock and forgets nothing: a machine not
    /// created yet has the memory of `host.toml`, one stopped runs nothing.
    #[test]
    fn a_claim_s_look_reads_without_the_lock_and_changes_nothing() {
        let resources = MachineResources::default();
        let memory = Memory::of(9216);
        let none = Host::default();
        assert_eq!(
            look_room(&none, &memory, &memory, &resources, 2048, 0),
            RoomLook::Fits
        );
        assert_eq!(none.calls(), Vec::<String>::new());
        let refused = look_room(&none, &memory, &memory, &resources, 8192, 0);
        assert!(matches!(refused, RoomLook::Refused(_)), "{refused:?}");
        let running = Host::with(&[(MACHINE_NAME, true)]);
        memory.run("dagq-q-run-r1", 6144);
        assert!(matches!(
            look_room(&running, &memory, &memory, &resources, 4096, 0),
            RoomLook::Wait(_)
        ));
        // Stopped, its containers are not read.
        let stopped = Host::with(&[(MACHINE_NAME, false)]);
        assert_eq!(
            look_room(&stopped, &memory, &memory, &resources, 4096, 0),
            RoomLook::Fits
        );
        // A wait of an owner over is not counted, nor forgotten.
        memory
            .waits
            .lock()
            .unwrap()
            .push(ask("dagq-q-run-r9", Purpose::Turn, 1, 9));
        memory.over.lock().unwrap().push(9);
        assert_eq!(
            look_room(&stopped, &memory, &memory, &resources, 4096, 0),
            RoomLook::Fits
        );
        assert_eq!(Memory::names(&memory.waits), ["dagq-q-run-r9"]);
    }

    /// A turn's reservation whose container has not started holds past
    /// the start's grace while the process that starts it lives: another's
    /// wait does not take its room by time alone.
    #[test]
    fn a_turn_s_reservation_before_its_start_holds_while_its_process_lives() {
        let host = Host::default();
        let memory = Memory::of(9216);
        let machine = MemoryMachine {
            users: &host,
            memory: &memory,
            records: &memory,
        };
        let slow = ask("dagq-q-run-r1", Purpose::Turn, 4096, 1);
        let held = ask("dagq-q-run-r2", Purpose::Turn, 4096, 2);
        try_reserve(&machine, &slow, 0).unwrap();
        try_reserve(&machine, &held, 0).unwrap();
        let other = ask("dagq-p-run-r3", Purpose::Turn, 4096, 3);
        assert!(matches!(
            try_reserve(&machine, &other, START_GRACE_SECS * 10).unwrap(),
            MemoryStep::Wait(_)
        ));
        assert_eq!(
            Memory::names(&memory.reservations),
            ["dagq-q-run-r1", "dagq-q-run-r2"]
        );
        // Its process is gone: its room is free.
        memory.over.lock().unwrap().push(1);
        assert_eq!(
            try_reserve(&machine, &other, START_GRACE_SECS * 10 + 1).unwrap(),
            MemoryStep::Reserved
        );
    }

    /// Two turns of different queues that begin to wait in the same second
    /// are served in the order they came, not by their containers' names:
    /// the place is given under the lock.
    #[test]
    fn waits_of_the_same_second_of_two_queues_are_served_in_the_order_they_came() {
        let host = Host::default();
        let memory = Memory::of(5120);
        let machine = MemoryMachine {
            users: &host,
            memory: &memory,
            records: &memory,
        };
        try_reserve(
            &machine,
            &ask("dagq-q-verify-v1", Purpose::Verification, 4096, 1),
            0,
        )
        .unwrap();
        let mut came_first = ask("dagq-b-run-1", Purpose::Turn, 4096, 2);
        came_first.owner = LeaseOwner::Run {
            db: "/q/b/queue.db".into(),
            run: "1".into(),
        };
        came_first.since = 7;
        let came_later = MemoryAsk {
            since: 7,
            ..ask("dagq-a-run-1", Purpose::Turn, 4096, 3)
        };
        assert!(matches!(
            try_reserve(&machine, &came_first, 7).unwrap(),
            MemoryStep::Wait(_)
        ));
        assert!(matches!(
            try_reserve(&machine, &came_later, 7).unwrap(),
            MemoryStep::Wait(_)
        ));
        let seqs: Vec<(String, u64)> = memory
            .waits
            .lock()
            .unwrap()
            .iter()
            .map(|wait| (wait.container.clone(), wait.seq))
            .collect();
        assert_eq!(
            seqs,
            [
                ("dagq-b-run-1".to_owned(), 1),
                ("dagq-a-run-1".to_owned(), 2)
            ]
        );
        release_memory(&machine, "dagq-q-verify-v1").unwrap();
        assert!(matches!(
            try_reserve(&machine, &came_later, 8).unwrap(),
            MemoryStep::Wait(why) if why.contains("dagq-b-run-1")
        ));
        assert_eq!(
            try_reserve(&machine, &came_first, 8).unwrap(),
            MemoryStep::Reserved
        );
    }

    /// The wait tells when it began, when it went past the long mark and
    /// how it ended, pausing between the tries; one reserved at once tells
    /// nothing; a stop read after a try ends it as [`MemoryWaitStopped`].
    #[test]
    fn a_wait_tells_its_start_its_long_mark_and_its_end() {
        let clock = std::cell::Cell::new(100_000);
        let now = || clock.get();
        let pause = || clock.set(clock.get() + 600_000);
        let go_on = || None;
        let mut notes = Vec::new();
        let mut tries = 0;
        wait_for_memory(
            |_| {
                tries += 1;
                Ok(if tries < 5 {
                    MemoryStep::Wait(format!("full {tries}"))
                } else {
                    MemoryStep::Reserved
                })
            },
            &now,
            &pause,
            &go_on,
            &mut |note| notes.push(format!("{note:?}")),
        )
        .unwrap();
        assert_eq!(notes.iter().filter(|note| *note == "Still").count(), 3);
        notes.retain(|note| note != "Still");
        assert_eq!(
            notes,
            [
                r#"Waiting { why: "full 1" }"#,
                r#"Long { why: "full 4" }"#,
                r#"Ended { outcome: "reserved", waited_secs: 2400 }"#,
            ]
        );
        notes.clear();
        wait_for_memory(
            |_| Ok(MemoryStep::Reserved),
            &now,
            &pause,
            &go_on,
            &mut |note| notes.push(format!("{note:?}")),
        )
        .unwrap();
        assert!(notes.is_empty());
        let mut first = true;
        let refused = wait_for_memory(
            |_| {
                Ok(if std::mem::take(&mut first) {
                    MemoryStep::Wait("full".into())
                } else {
                    MemoryStep::Refused("never".into())
                })
            },
            &now,
            &pause,
            &go_on,
            &mut |note| notes.push(format!("{note:?}")),
        )
        .unwrap_err();
        assert_eq!(refused.to_string(), "never");
        assert_eq!(
            notes.last().unwrap(),
            r#"Ended { outcome: "refused", waited_secs: 600 }"#
        );
        // Stopped while it waits: no more tries, and why.
        let tries = std::cell::Cell::new(0);
        let stop = || (tries.get() >= 2).then(|| "task 3 is held".to_owned());
        let stopped = wait_for_memory(
            |_| {
                tries.set(tries.get() + 1);
                Ok(MemoryStep::Wait("full".into()))
            },
            &now,
            &pause,
            &stop,
            &mut |note| notes.push(format!("{note:?}")),
        )
        .unwrap_err();
        assert_eq!(
            stopped.downcast_ref::<MemoryWaitStopped>(),
            Some(&MemoryWaitStopped("task 3 is held".into()))
        );
        assert_eq!(tries.get(), 2);
        assert_eq!(
            notes.last().unwrap(),
            r#"Ended { outcome: "stopped", waited_secs: 600 }"#
        );
    }
}
