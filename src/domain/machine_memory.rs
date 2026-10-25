//! The memory of the dagq machine the containers reserve before they start
//! (ADR-t3163-1): a worker's turn and a verification of `integrate` take
//! their container's memory limit out of the machine's budget under the
//! machine's host-wide lock, and wait outside it while it does not fit.
//! What is decided here: the budget ([`budget`]: the machine's memory less
//! its base, [`BASE_MIB`] and [`RELAY_MIB`] for each relay), whether a
//! container fits ([`judge`]), the order of the waits ([`ahead`],
//! [`next_seq`]), which reservation and wait still hold
//! ([`reservation_held`], [`wait_held`]), what a wait does next
//! ([`wait_next`]), what a new claim waits for ([`claim_wait`]) and how a
//! run's events tell of its wait ([`standing_wait`]). The lock, the records and podman are the
//! adapters'.

use std::cmp::Ordering;

use serde::{Deserialize, Serialize};

use super::podman_machine::LeaseOwner;
use super::{EventKind, RunEvent};

/// What the machine itself uses of its memory besides the containers, in
/// MiB: 1 GiB. The machine is a Fedora CoreOS VM whose kernel, systemd,
/// podman's service and the containers' conmon processes take some 500 to
/// 700 MiB at rest; the rest is room for the page cache the builds' files
/// go through. It is the runtime's own part, the same for every project,
/// so `host.toml` has no key for it (ADR-t3163-1 decision 2).
pub const BASE_MIB: u64 = 1024;

/// What one running relay ([`super::broker_relay`]) takes of the machine,
/// in MiB: `dagq relay serve` is one small process of the client and its
/// conmon (some 10 to 20 MiB), rounded up to 64 MiB so that a relay that
/// buffers a large answer still fits. Relays have no memory limit and are
/// counted here, never as a container's limit.
pub const RELAY_MIB: u64 = 64;

/// How long a turn's reservation is kept after its container was started
/// ([`MemoryAsk::started`]) before the container is seen on the machine:
/// 2 minutes. `podman run` makes the container within seconds of its
/// spawn, so a started reservation older than this whose container is not
/// on the machine is of a container that ended. Before its start a
/// reservation is never ended by time: it holds while the process that
/// starts it lives.
pub const START_GRACE_SECS: i64 = 120;

/// How long a container may wait for its memory before the inbox is told
/// (`container_memory_wait_long`): 30 minutes, about two turns' builds of
/// a project's worker. A wait as long as that is no longer a peak of the
/// runs passing: a machine or a limit too small for the work, or a
/// container that does not end.
pub const LONG_WAIT_SECS: i64 = 30 * 60;

/// How often a waiting container asks again: 5 seconds. Each try takes the
/// machine's lock and reads the machine (a few podman commands), and a
/// turn or a verification that waits does not mind seconds.
pub const POLL_SECS: u64 = 5;

/// What a reservation is for. The waits are served verifications first:
/// a verification lands work that is done, a turn starts new work, and
/// landing first leaves fewer runs in progress (ADR-t3163-1 decision 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Purpose {
    Verification,
    Turn,
}

impl Purpose {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Verification => "verification",
            Self::Turn => "turn",
        }
    }
}

/// One container's ask for memory: a reservation once it fits, a wait
/// until then. A turn's is held while the process that starts its
/// container lives, then while the container is on the machine
/// ([`reservation_held`]); a verification's from before its first command
/// to its cleanup, the commands' containers coming and going between, while
/// its process lives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryAsk {
    /// The name of the container it is for: a turn's
    /// ([`super::podman_machine::QueueKey::run_container`]) or a
    /// verification's ([`super::podman_machine::QueueKey::verification_container`]).
    pub container: String,
    pub purpose: Purpose,
    /// The container's memory limit (`[container]` of `host.toml`).
    pub memory_mib: u64,
    /// The run it is for; over once the run is ([`super::podman_machine::lease_held`]).
    pub owner: LeaseOwner,
    /// The process that waits for it, and holds a verification's
    /// reservation: over once it is.
    pub pid: u32,
    /// When it was taken or began to wait (Unix seconds).
    pub since: i64,
    /// Its place in the host-wide order of the waits ([`next_seq`]), given
    /// under the machine's lock when it first waits: two waits that began
    /// in the same second keep the order they came in. 0 for an ask that
    /// has not waited.
    #[serde(default)]
    pub seq: u64,
    /// When a turn's container was started on its reservation (Unix
    /// seconds), `None` before: the turn's reservation ends with its
    /// container only after it ([`reservation_held`]).
    #[serde(default)]
    pub started: Option<i64>,
}

/// What one try at a container's memory came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryStep {
    /// Reserved: the container may start.
    Reserved,
    /// It waits, for why; its wait is recorded in the host-wide order.
    Wait(String),
    /// It never fits: the container cannot be used, and runs nowhere else.
    Refused(String),
}

/// What a wait for a container's memory tells as it goes, to be recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryNote<'a> {
    /// It began to wait, for why.
    Waiting { why: &'a str },
    /// It waits on after another try: nothing to record, but what waits
    /// shows it lives (a session wrapper's heartbeat).
    Still,
    /// It has waited [`LONG_WAIT_SECS`], for why now.
    Long { why: &'a str },
    /// It ended after waiting `waited_secs`: `reserved`, `refused`,
    /// `failed` or `stopped` (its turn is to end, [`WaitMove::Stop`]).
    Ended {
        outcome: &'static str,
        waited_secs: i64,
    },
}

/// A dagq container the machine runs: its name and memory limit (`None`
/// for one without), and whether it is a relay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineContainer {
    pub name: String,
    pub memory_mib: Option<u64>,
    pub relay: bool,
}

/// What the containers may take of a machine of `machine_mib` running
/// `relays` relays.
pub const fn budget(machine_mib: u64, relays: usize) -> u64 {
    machine_mib
        .saturating_sub(BASE_MIB)
        .saturating_sub(RELAY_MIB.saturating_mul(relays as u64))
}

/// What one try at an ask comes to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Judgement {
    /// It fits: reserve it.
    Reserve,
    /// It does not fit now, or another waits before it: wait.
    Wait(String),
    /// It never fits: its limit is beyond the machine's budget, and the
    /// container cannot be used (it never runs on the host instead).
    Refuse(String),
}

/// What the machine's memory is taken by now: the reservations and the
/// dagq containers no reservation names, but `me`'s own container, which
/// is counted in `me`'s ask. A relay is in the base; a container with no
/// limit takes the whole machine, as nothing bounds it.
fn used(
    machine_mib: u64,
    containers: &[MachineContainer],
    reservations: &[MemoryAsk],
    me: Option<&str>,
) -> u64 {
    let reserved: u64 = reservations
        .iter()
        .filter(|held| Some(held.container.as_str()) != me)
        .map(|held| held.memory_mib)
        .sum();
    let unrecorded: u64 = containers
        .iter()
        .filter(|container| !container.relay)
        .filter(|container| Some(container.name.as_str()) != me)
        .filter(|container| {
            !reservations
                .iter()
                .any(|held| held.container == container.name)
        })
        .map(|container| container.memory_mib.unwrap_or(machine_mib))
        .sum();
    reserved.saturating_add(unrecorded)
}

/// Why a limit of `memory_mib` never fits a budget of `room` out of a
/// machine of `machine_mib`, `None` when it may.
fn never_fits(machine_mib: u64, room: u64, memory_mib: u64) -> Option<String> {
    (memory_mib > room).then(|| {
        format!(
            "the container's memory limit of {memory_mib} MiB is beyond what the dagq machine's \
{machine_mib} MiB leaves the containers ({room} MiB, after {BASE_MIB} MiB for the machine and \
{RELAY_MIB} MiB for each relay): make the machine larger or the limit smaller in host.toml"
        )
    })
}

/// The ask among `waits` that comes before `me`: a verification before a
/// turn, then the one that began to wait first, whatever queue it is of.
pub fn ahead<'a>(waits: &'a [MemoryAsk], me: &MemoryAsk) -> Option<&'a MemoryAsk> {
    waits
        .iter()
        .filter(|wait| wait.container != me.container)
        .filter(|wait| order(wait, me) == Ordering::Less)
        .min_by(|a, b| order(a, b))
}

/// The order of two waits: verifications first, then by their place in
/// the host-wide order ([`MemoryAsk::seq`]: by when they came, the same
/// second's too), the container's name last so that the order is total.
pub fn order(a: &MemoryAsk, b: &MemoryAsk) -> Ordering {
    (a.purpose, a.seq, a.since, &a.container).cmp(&(b.purpose, b.seq, b.since, &b.container))
}

/// The place in the host-wide order of `container`'s wait, `waits` being
/// every wait recorded (read under the machine's lock): its own when it
/// waits already, else one after the last there, so that a wait that comes
/// later comes after, whatever second it came in.
pub fn next_seq(waits: &[MemoryAsk], container: &str) -> u64 {
    waits
        .iter()
        .find(|wait| wait.container == container)
        .map_or_else(
            || {
                waits
                    .iter()
                    .map(|wait| wait.seq)
                    .max()
                    .unwrap_or(0)
                    .saturating_add(1)
            },
            |wait| wait.seq,
        )
}

/// Whether `me` may take its memory now, the machine having `machine_mib`
/// and running `containers`, with `reservations` held and `waits` waiting
/// (both of every queue of the host, those over already left out): it is
/// refused when its limit never fits, waits while another wait comes before
/// it or what is taken and its limit go beyond the budget, and is reserved
/// otherwise. A reservation or a wait of its own container is its own.
pub fn judge(
    machine_mib: u64,
    containers: &[MachineContainer],
    reservations: &[MemoryAsk],
    waits: &[MemoryAsk],
    me: &MemoryAsk,
) -> Judgement {
    let relays = containers
        .iter()
        .filter(|container| container.relay)
        .count();
    let room = budget(machine_mib, relays);
    if let Some(why) = never_fits(machine_mib, room, me.memory_mib) {
        return Judgement::Refuse(why);
    }
    if let Some(first) = ahead(waits, me) {
        return Judgement::Wait(format!(
            "the {} of container {} waits for the dagq machine's memory before it",
            first.purpose.as_str(),
            first.container
        ));
    }
    let taken = used(machine_mib, containers, reservations, Some(&me.container));
    if taken.saturating_add(me.memory_mib) > room {
        return Judgement::Wait(format!(
            "the dagq machine's containers take {taken} MiB of the {room} MiB it leaves them, and \
the container's memory limit of {} MiB does not fit beside them",
            me.memory_mib
        ));
    }
    Judgement::Reserve
}

/// Why a new claim of a worker whose container's limit is `memory_mib`
/// waits ([`judge`] as if it asked now, after every wait), `None` when it
/// fits: a wait already there holds every new claim, which would only
/// queue behind it (ADR-t3163-1 decision 4). A limit that never fits is the
/// container's to refuse, not the claim's to wait for: `None`.
pub fn claim_wait(
    machine_mib: u64,
    containers: &[MachineContainer],
    reservations: &[MemoryAsk],
    waits: &[MemoryAsk],
    memory_mib: u64,
) -> Option<String> {
    let relays = containers
        .iter()
        .filter(|container| container.relay)
        .count();
    let room = budget(machine_mib, relays);
    if never_fits(machine_mib, room, memory_mib).is_some() {
        return None;
    }
    if let Some(first) = waits.iter().min_by(|a, b| order(a, b)) {
        return Some(format!(
            "{} container(s) wait for the dagq machine's memory, the {} of container {} first",
            waits.len(),
            first.purpose.as_str(),
            first.container
        ));
    }
    let taken = used(machine_mib, containers, reservations, None);
    (taken.saturating_add(memory_mib) > room).then(|| {
        format!(
            "the dagq machine's containers take {taken} MiB of the {room} MiB it leaves them, and \
a worker's container of {memory_mib} MiB does not fit beside them"
        )
    })
}

/// Why no container of `memory_mib` can ever run on a machine of
/// `machine_mib` running `relays` relays ([`Judgement::Refuse`]), `None`
/// when it can.
pub fn refusal(machine_mib: u64, relays: usize, memory_mib: u64) -> Option<String> {
    never_fits(machine_mib, budget(machine_mib, relays), memory_mib)
}

/// Whether the reservation `held` still holds, its owner run seen as
/// `owner_held` and its process as `pid_alive`, its container being on
/// the machine or not (`on_machine`) at `now`. Any reservation ends with
/// its run. A turn's holds before its container's start while the process
/// that starts it lives, however long that takes; once started
/// ([`MemoryAsk::started`]), it ends when its container, past
/// [`START_GRACE_SECS`] from the start, is not on the machine: it ended,
/// and a reservation its wrapper kept alive does not hold the machine. A
/// verification's holds while its process lives, whether a command's
/// container runs or not: its commands start one container each, and the
/// room between them is its own.
pub fn reservation_held(
    held: &MemoryAsk,
    owner_held: bool,
    pid_alive: bool,
    on_machine: bool,
    now: i64,
) -> bool {
    if !owner_held {
        return false;
    }
    match (held.purpose, held.started) {
        (Purpose::Verification, _) | (Purpose::Turn, None) => pid_alive,
        (Purpose::Turn, Some(started)) => {
            on_machine || now.saturating_sub(started) < START_GRACE_SECS
        }
    }
}

/// What one try at an ask came to, as a wait sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tried {
    Reserved,
    Wait(String),
    Refused(String),
    /// The try failed (the lock or the records).
    Failed,
}

/// What a wait has told so far: when it began to wait (Unix
/// milliseconds), `None` while it has not, and whether it told of its
/// long wait.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WaitSoFar {
    pub began_ms: Option<i64>,
    pub told_long: bool,
}

/// What a wait does after a try.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitMove {
    /// Reserved: the container starts.
    Start,
    /// Try again after a pause.
    Pause,
    /// Its limit never fits, for why.
    Refuse(String),
    /// The try failed.
    Fail,
    /// What waits is to end, for why (its run's stop, a hold or a
    /// replanning that fences its task): what it reserved or waits for is
    /// forgotten and its container does not start.
    Stop(String),
}

/// What a wait records and does after a try at `now_ms` that came to
/// `tried`, `stop` saying why what waits is to end, if it is (read after
/// the try, so that one reserved is stopped too): the notes to tell, in
/// order, the move and what it has told so far. Nothing is told of an ask
/// reserved at its first try; [`MemoryNote::Long`] once, at
/// [`LONG_WAIT_SECS`] from its start.
pub fn wait_next<'a>(
    so_far: WaitSoFar,
    now_ms: i64,
    tried: &'a Tried,
    stop: Option<&str>,
) -> (Vec<MemoryNote<'a>>, WaitMove, WaitSoFar) {
    let ended = |outcome| {
        so_far.began_ms.map(|began| MemoryNote::Ended {
            outcome,
            waited_secs: now_ms.saturating_sub(began) / 1000,
        })
    };
    if let Some(why) = stop {
        return (
            ended("stopped").into_iter().collect(),
            WaitMove::Stop(why.to_owned()),
            so_far,
        );
    }
    match tried {
        Tried::Reserved => (
            ended("reserved").into_iter().collect(),
            WaitMove::Start,
            so_far,
        ),
        Tried::Refused(why) => (
            ended("refused").into_iter().collect(),
            WaitMove::Refuse(why.clone()),
            so_far,
        ),
        Tried::Failed => (
            ended("failed").into_iter().collect(),
            WaitMove::Fail,
            so_far,
        ),
        Tried::Wait(why) => {
            let mut notes = Vec::new();
            let began = match so_far.began_ms {
                Some(began) => {
                    notes.push(MemoryNote::Still);
                    began
                }
                None => {
                    notes.push(MemoryNote::Waiting { why });
                    now_ms
                }
            };
            let long = !so_far.told_long
                && now_ms.saturating_sub(began) >= LONG_WAIT_SECS.saturating_mul(1000);
            if long {
                notes.push(MemoryNote::Long { why });
            }
            (
                notes,
                WaitMove::Pause,
                WaitSoFar {
                    began_ms: Some(began),
                    told_long: so_far.told_long || long,
                },
            )
        }
    }
}

/// Whether the wait `wait` still holds: while its run and the process
/// that waits live.
pub const fn wait_held(owner_held: bool, pid_alive: bool) -> bool {
    owner_held && pid_alive
}

/// The kinds of a run's wait for the machine's memory, of its start
/// ([`EventKind::ContainerMemoryWaiting`]), of its passing
/// [`LONG_WAIT_SECS`] ([`EventKind::ContainerMemoryWaitLong`]) and of its
/// end ([`EventKind::ContainerMemoryWaitEnded`]).
pub const WAIT_KINDS: [EventKind; 3] = [
    EventKind::ContainerMemoryWaiting,
    EventKind::ContainerMemoryWaitLong,
    EventKind::ContainerMemoryWaitEnded,
];

/// The run's wait for the machine's memory that stands at the end of
/// `events` (in order): its latest `container_memory_waiting` or
/// `container_memory_wait_long` with nothing after it that ends it (its
/// `container_memory_wait_ended`, the next turn's start, a new session of
/// the run, the first verification command, or the run's recovery, which
/// tell of a wait whose process was gone before it said so). `None` while
/// the run waits for no memory.
pub fn standing_wait<E: std::borrow::Borrow<RunEvent>>(events: &[E]) -> Option<&RunEvent> {
    let last = events.iter().map(E::borrow).rposition(|event| {
        event.kind.is_in(&WAIT_KINDS)
            || matches!(
                event.kind.known(),
                Some(
                    EventKind::TurnStarted
                        | EventKind::AgentStarted
                        | EventKind::ResumeStarted
                        | EventKind::WrapperLaunched
                        | EventKind::VerificationCommand
                        | EventKind::RunRecovered
                        | EventKind::RunIntegrated
                )
            )
    })?;
    let event = events[last].borrow();
    matches!(
        event.kind.known(),
        Some(EventKind::ContainerMemoryWaiting | EventKind::ContainerMemoryWaitLong)
    )
    .then_some(event)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::json;

    use super::*;

    fn run_owner(db: &str) -> LeaseOwner {
        LeaseOwner::Run {
            db: PathBuf::from(db),
            run: "r".into(),
        }
    }

    fn ask(container: &str, purpose: Purpose, memory_mib: u64, since: i64) -> MemoryAsk {
        MemoryAsk {
            container: container.into(),
            purpose,
            memory_mib,
            owner: run_owner("/q/a/queue.db"),
            pid: 1,
            since,
            seq: u64::try_from(since).unwrap_or(0),
            started: None,
        }
    }

    fn container(name: &str, memory_mib: u64) -> MachineContainer {
        MachineContainer {
            name: name.into(),
            memory_mib: Some(memory_mib),
            relay: false,
        }
    }

    fn relay(name: &str) -> MachineContainer {
        MachineContainer {
            name: name.into(),
            memory_mib: None,
            relay: true,
        }
    }

    /// The budget is the machine's memory less its base and each relay's
    /// share; what runs or is reserved and the ask together must fit it.
    #[test]
    fn a_container_fits_when_what_is_taken_and_its_limit_fit_the_machine_less_its_base() {
        let me = ask("dagq-q-run-r2", Purpose::Turn, 4096, 10);
        // 10240 − 1024 − 64 = 9152 left: 4096 reserved + 4096 fits, 5120
        // reserved does not.
        let relays = [relay("dagq-q-relay-r1")];
        let held = [ask("dagq-q-run-r1", Purpose::Turn, 4096, 1)];
        assert_eq!(budget(10240, 1), 9152);
        assert_eq!(judge(10240, &relays, &held, &[], &me), Judgement::Reserve);
        let held = [ask("dagq-q-run-r1", Purpose::Turn, 5120, 1)];
        let Judgement::Wait(why) = judge(10240, &relays, &held, &[], &me) else {
            panic!("fitted beside 5120 MiB");
        };
        assert!(why.contains("take 5120 MiB of the 9152 MiB"), "{why}");
        // A relay is in the base, never counted as a container's limit.
        let relays: Vec<MachineContainer> = (0..3)
            .map(|n| relay(&format!("dagq-q-relay-r{n}")))
            .collect();
        assert_eq!(budget(10240, 3), 9024);
        let held = [ask("dagq-q-run-r1", Purpose::Turn, 4928, 1)];
        assert_eq!(judge(10240, &relays, &held, &[], &me), Judgement::Reserve);
        let held = [ask("dagq-q-run-r1", Purpose::Turn, 4929, 1)];
        assert!(matches!(
            judge(10240, &relays, &held, &[], &me),
            Judgement::Wait(_)
        ));
    }

    /// A reservation's own container on the machine is counted in the
    /// reservation, once; a dagq container no reservation names is counted
    /// with its limit, and one without a limit takes the whole machine.
    #[test]
    fn a_reserved_container_counts_once_and_an_unrecorded_one_with_its_limit() {
        let me = ask("dagq-q-run-r2", Purpose::Turn, 4096, 10);
        let held = [ask("dagq-q-verify-v1", Purpose::Verification, 4096, 1)];
        let running = [container("dagq-q-verify-v1", 4096)];
        assert_eq!(judge(9216, &running, &held, &[], &me), Judgement::Reserve);
        // Unrecorded: the same container with no reservation is counted by
        // its limit, and with the reservation of another it is too much.
        let other = [ask("dagq-p-run-x", Purpose::Turn, 1024, 1)];
        assert!(matches!(
            judge(9216, &running, &other, &[], &me),
            Judgement::Wait(_)
        ));
        assert_eq!(judge(9216, &running, &[], &[], &me), Judgement::Reserve);
        // A container without a limit leaves no room.
        let unbounded = [MachineContainer {
            memory_mib: None,
            ..container("dagq-q-run-r9", 0)
        }];
        assert!(matches!(
            judge(65536, &unbounded, &[], &[], &me),
            Judgement::Wait(_)
        ));
        // Its own container (left by an earlier turn) and its own
        // reservation are its own, not another's.
        let mine = [container("dagq-q-run-r2", 4096)];
        let held = [ask("dagq-q-run-r2", Purpose::Turn, 4096, 1)];
        assert_eq!(judge(6144, &mine, &held, &[], &me), Judgement::Reserve);
    }

    /// A limit beyond the budget never fits: refused at once, with why, and
    /// never waited for; the base larger than the machine less the limit
    /// is such a case.
    #[test]
    fn a_limit_beyond_the_budget_is_refused_without_a_wait() {
        let me = ask("dagq-q-run-r2", Purpose::Turn, 8192, 10);
        // 9216 − 1024 = 8192: it fits, alone.
        assert_eq!(judge(9216, &[], &[], &[], &me), Judgement::Reserve);
        // A relay's share makes the machine too small for it.
        let Judgement::Refuse(why) = judge(9216, &[relay("dagq-q-relay-r1")], &[], &[], &me) else {
            panic!("not refused");
        };
        assert!(why.contains("8192 MiB is beyond"), "{why}");
        assert!(why.contains("host.toml"), "{why}");
        // Refused even with waits before it: waiting never makes it fit.
        let waits = [ask("dagq-q-verify-v1", Purpose::Verification, 1024, 1)];
        assert!(matches!(
            judge(8192, &[], &[], &waits, &me),
            Judgement::Refuse(_)
        ));
        assert!(refusal(8192, 0, 8192).is_some());
        assert!(refusal(9216, 0, 8192).is_none());
        // A claim does not wait for what the container refuses.
        assert_eq!(claim_wait(8192, &[], &[], &[], 8192), None);
    }

    /// The waits are served verifications first, then by when they began,
    /// across every queue of the host: a turn of one queue does not pass a
    /// wait of another's that began before it.
    #[test]
    fn waits_go_verifications_first_then_by_when_they_began_whatever_their_queue() {
        let other_queue = |mut wait: MemoryAsk| {
            wait.owner = run_owner("/q/b/queue.db");
            wait
        };
        let turn_a = ask("dagq-a-run-1", Purpose::Turn, 1024, 5);
        let turn_b = other_queue(ask("dagq-b-run-1", Purpose::Turn, 1024, 3));
        let verify_a = ask("dagq-a-verify-1", Purpose::Verification, 1024, 9);
        let waits = [turn_a.clone(), turn_b.clone(), verify_a.clone()];
        // The verification goes first though it began last.
        assert_eq!(ahead(&waits, &verify_a), None);
        assert_eq!(ahead(&waits, &turn_b), Some(&verify_a));
        assert_eq!(ahead(&waits, &turn_a), Some(&verify_a));
        // Among the turns, the one of the other queue began first.
        let turns = [turn_a.clone(), turn_b.clone()];
        assert_eq!(ahead(&turns, &turn_a), Some(&turn_b));
        assert_eq!(ahead(&turns, &turn_b), None);
        let Judgement::Wait(why) = judge(65536, &[], &[], &turns, &turn_a) else {
            panic!("passed the wait before it");
        };
        assert!(why.contains("dagq-b-run-1"), "{why}");
        assert_eq!(judge(65536, &[], &[], &turns, &turn_b), Judgement::Reserve);
        // A new turn's wait goes after the ones there.
        let late = ask("dagq-a-run-2", Purpose::Turn, 1024, 20);
        let mut sorted = [late, turn_a, turn_b, verify_a];
        sorted.sort_by(order);
        let names: Vec<&str> = sorted.iter().map(|w| w.container.as_str()).collect();
        assert_eq!(
            names,
            [
                "dagq-a-verify-1",
                "dagq-b-run-1",
                "dagq-a-run-1",
                "dagq-a-run-2"
            ]
        );
    }

    /// A new claim waits while a wait stands or a worker's container would
    /// not fit, and goes on once it fits.
    #[test]
    fn a_claim_waits_while_another_waits_or_its_container_would_not_fit() {
        let held = [ask("dagq-q-run-r1", Purpose::Turn, 4096, 1)];
        assert_eq!(claim_wait(10240, &[], &held, &[], 4096), None);
        let why = claim_wait(10240, &[], &held, &[], 8192).unwrap();
        assert!(why.contains("does not fit"), "{why}");
        let waits = [ask("dagq-q-verify-v", Purpose::Verification, 1024, 3)];
        let why = claim_wait(65536, &[], &[], &waits, 1024).unwrap();
        assert!(why.contains("1 container(s) wait"), "{why}");
        assert!(why.contains("dagq-q-verify-v"), "{why}");
    }

    /// A turn's reservation holds while the process that starts its
    /// container lives, however long its start takes, and once started ends
    /// with its container: when it is not on the machine past the start's
    /// grace, or with its run. A verification's lasts from its first
    /// command to its cleanup whether a command's container runs or not,
    /// and ends with its process or its run.
    #[test]
    fn a_turn_s_reservation_ends_with_its_container_and_a_verification_s_with_its_process() {
        let turn = ask("dagq-q-run-r1", Purpose::Turn, 4096, 100);
        // Reserved, its container not started yet: no time ends it while
        // the process that starts it lives.
        assert!(reservation_held(&turn, true, true, false, 100));
        assert!(reservation_held(&turn, true, true, false, 100_000));
        assert!(!reservation_held(&turn, true, false, false, 100));
        let turn = MemoryAsk {
            started: Some(200),
            ..turn
        };
        // Started, its container not seen yet.
        assert!(reservation_held(
            &turn,
            true,
            true,
            false,
            200 + START_GRACE_SECS - 1
        ));
        // Started and running, however long, its wrapper gone or not.
        assert!(reservation_held(&turn, true, false, true, 100_000));
        // Its container ended (or never came).
        assert!(!reservation_held(
            &turn,
            true,
            true,
            false,
            200 + START_GRACE_SECS
        ));
        // Its run is over.
        assert!(!reservation_held(&turn, false, true, true, 100));

        let verification = ask("dagq-q-verify-v1", Purpose::Verification, 4096, 100);
        // Between two commands no container runs: still held.
        assert!(reservation_held(&verification, true, true, false, 100_000));
        assert!(reservation_held(&verification, true, true, true, 100_000));
        // Its process is gone, or its run is over.
        assert!(!reservation_held(&verification, true, false, true, 100));
        assert!(!reservation_held(&verification, false, true, true, 100));

        assert!(wait_held(true, true));
        assert!(!wait_held(true, false));
        assert!(!wait_held(false, true));
    }

    /// Two waits of the same kind that came in the same second keep the
    /// order they came in, whatever their queue or container's name: the
    /// place given under the lock decides, not the name.
    #[test]
    fn waits_of_the_same_second_keep_the_order_they_came_in_across_queues() {
        let mut first = ask("dagq-b-run-1", Purpose::Turn, 1024, 7);
        first.owner = run_owner("/q/b/queue.db");
        first.seq = next_seq(&[], &first.container);
        let waits = vec![first.clone()];
        let mut later = ask("dagq-a-run-1", Purpose::Turn, 1024, 7);
        later.seq = next_seq(&waits, &later.container);
        assert_eq!((first.seq, later.seq), (1, 2));
        // Its own wait keeps its place.
        assert_eq!(next_seq(&waits, "dagq-b-run-1"), 1);
        let waits = [first.clone(), later.clone()];
        assert_eq!(ahead(&waits, &later), Some(&first));
        assert_eq!(ahead(&waits, &first), None);
        let Judgement::Wait(why) = judge(65536, &[], &[], &waits, &later) else {
            panic!("passed the wait of the same second before it");
        };
        assert!(why.contains("dagq-b-run-1"), "{why}");
        assert_eq!(judge(65536, &[], &[], &waits, &first), Judgement::Reserve);
    }

    /// A wait tells its start once, a try after that as `Still`, its long
    /// mark once at exactly [`LONG_WAIT_SECS`] (not a millisecond before)
    /// and its end; one reserved at its first try tells nothing, and a stop
    /// read after a try ends it, reserved or not.
    #[test]
    fn a_wait_s_next_move_follows_its_try_its_time_and_its_stop() {
        let long_ms = LONG_WAIT_SECS * 1000;
        let wait = Tried::Wait("short".into());
        let (notes, then, so_far) = wait_next(WaitSoFar::default(), 1_000, &Tried::Reserved, None);
        assert_eq!((notes, then), (vec![], WaitMove::Start));
        assert_eq!(so_far, WaitSoFar::default());

        let (notes, then, so_far) = wait_next(WaitSoFar::default(), 1_000, &wait, None);
        assert_eq!(notes, [MemoryNote::Waiting { why: "short" }]);
        assert_eq!(then, WaitMove::Pause);
        assert_eq!(so_far.began_ms, Some(1_000));
        let (notes, _, so_far) = wait_next(so_far, 1_000 + long_ms - 1, &wait, None);
        assert_eq!(notes, [MemoryNote::Still]);
        assert!(!so_far.told_long);
        let (notes, _, so_far) = wait_next(so_far, 1_000 + long_ms, &wait, None);
        assert_eq!(
            notes,
            [MemoryNote::Still, MemoryNote::Long { why: "short" }]
        );
        assert!(so_far.told_long);
        let (notes, _, so_far) = wait_next(so_far, 1_000 + 2 * long_ms, &wait, None);
        assert_eq!(notes, [MemoryNote::Still]);

        let ended = |outcome| MemoryNote::Ended {
            outcome,
            waited_secs: LONG_WAIT_SECS * 2,
        };
        let at = 1_000 + 2 * long_ms;
        let (notes, then, _) = wait_next(so_far, at, &Tried::Reserved, None);
        assert_eq!((notes, then), (vec![ended("reserved")], WaitMove::Start));
        let refused = Tried::Refused("never".into());
        let (notes, then, _) = wait_next(so_far, at, &refused, None);
        assert_eq!(
            (notes, then),
            (vec![ended("refused")], WaitMove::Refuse("never".into()))
        );
        let (notes, then, _) = wait_next(so_far, at, &Tried::Failed, None);
        assert_eq!((notes, then), (vec![ended("failed")], WaitMove::Fail));
        // A stop ends it, a reservation just taken too.
        let (notes, then, _) = wait_next(so_far, at, &Tried::Reserved, Some("held"));
        assert_eq!(
            (notes, then),
            (vec![ended("stopped")], WaitMove::Stop("held".into()))
        );
        let (notes, then, _) = wait_next(WaitSoFar::default(), 5, &Tried::Reserved, Some("held"));
        assert_eq!((notes, then), (vec![], WaitMove::Stop("held".into())));
    }

    fn event(id: i64, kind: EventKind) -> RunEvent {
        RunEvent {
            id: super::super::EventId::new(id),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.into(),
            payload: json!({}),
            created_at: "2026-10-25T00:00:00.000Z".into(),
            actor: None,
        }
    }

    /// A wait stands from its start, through its passing the long mark, to
    /// its end or to what tells of the run going on without it.
    #[test]
    fn a_run_s_wait_stands_until_it_ends_or_the_run_goes_on() {
        use EventKind::*;
        assert!(standing_wait(&[event(1, TurnFinished)]).is_none());
        let waiting = [event(1, TurnFinished), event(2, ContainerMemoryWaiting)];
        assert_eq!(standing_wait(&waiting).map(|e| e.id.as_i64()), Some(2));
        let long = [
            event(1, ContainerMemoryWaiting),
            event(2, TurnRequested),
            event(3, ContainerMemoryWaitLong),
        ];
        assert_eq!(standing_wait(&long).map(|e| e.id.as_i64()), Some(3));
        for end in [
            ContainerMemoryWaitEnded,
            TurnStarted,
            VerificationCommand,
            RunRecovered,
            ResumeStarted,
        ] {
            let ended = [event(1, ContainerMemoryWaiting), event(2, end)];
            assert!(standing_wait(&ended).is_none(), "{end:?}");
        }
    }
}
