//! The dagq podman machine (ADR-t2113-1 decisions 4 to 6, ADR-t2173-1):
//! the one Podman machine of the host that the containers of the worker,
//! its resume and the verification of `integrate` run in, what is decided
//! of it before a container starts ([`prepare`]) and when its last user
//! leaves ([`stops`]), the host directories it mounts ([`MachineMounts`],
//! [`mounts_mismatch`]), its resources ([`MachineResources`]) and the
//! named volumes inside it ([`QueueKey`]). The commands and the lock are
//! the adapters'; this is what they decide with.

use std::path::{Component, Path, PathBuf};

/// The name of the dagq machine, and of the connection podman makes for
/// it: every podman command dagq runs names it, so the person's default
/// machine and default connection are never the ones acted on.
pub const MACHINE_NAME: &str = "dagq";

/// The last component of each kind of host directory the machine mounts:
/// a queue's run directories (`<queue dir>/runs`), the parent of its runs'
/// clones and the parent of its verifications' clones.
pub const MOUNT_DIR_NAMES: [&str; 3] = ["runs", "clones", "verification"];

/// The CPUs, memory and disk the dagq machine is created with, from the
/// `[machine]` table of the host-wide `host.toml` (`cpus`, `memory_mib`,
/// `disk_gib`). The machine is one per host and every project on it shares
/// it, so these are the host's values, not a project's (ADR-t2257-1
/// decision 6). They apply when the machine is created: a changed value
/// takes effect when the machine is created again.
///
/// The defaults are modest values meant to suit any project on a common
/// development host, not values measured on dagq's own build: 2 CPUs and
/// 4 GiB leave most of an 8-core / 16 GiB host to the person and to the
/// host's own runs, and are about what a small project's build and tests
/// in one container need; the disk is sparse, so 60 GiB costs only what
/// the build outputs and caches inside it take. A project whose build
/// needs more says so in `host.toml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MachineResources {
    pub cpus: u32,
    pub memory_mib: u64,
    pub disk_gib: u64,
}

pub const DEFAULT_MACHINE_CPUS: u32 = 2;
pub const DEFAULT_MACHINE_MEMORY_MIB: u64 = 4096;
pub const DEFAULT_MACHINE_DISK_GIB: u64 = 60;

impl Default for MachineResources {
    fn default() -> Self {
        Self {
            cpus: DEFAULT_MACHINE_CPUS,
            memory_mib: DEFAULT_MACHINE_MEMORY_MIB,
            disk_gib: DEFAULT_MACHINE_DISK_GIB,
        }
    }
}

/// The host directories of one queue the machine mounts (ADR-t2173-1
/// decision 1): its run directories, the parent of its runs' clones and
/// the parent of its verifications' clones, each at the same path inside
/// the machine. Nothing else of the host is mounted: not `$HOME`, not the
/// worker's cache or the build outputs (named volumes inside the machine)
/// and not the worktrees the landing uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineMounts {
    pub runs: PathBuf,
    pub clones: PathBuf,
    pub verifications: PathBuf,
}

impl MachineMounts {
    /// The three directories of the queue whose directory is `queue_dir`.
    pub fn of_queue(queue_dir: &Path) -> Self {
        let [runs, clones, verifications] = MOUNT_DIR_NAMES.map(|name| queue_dir.join(name));
        Self {
            runs,
            clones,
            verifications,
        }
    }

    pub fn dirs(&self) -> [&Path; 3] {
        [&self.runs, &self.clones, &self.verifications]
    }

    /// The directory the queues are under (`<data home>/dagq`): the parent
    /// of the queue directory, each queue's directory being one of its
    /// entries.
    pub fn data_root(&self) -> &Path {
        self.runs
            .parent()
            .and_then(Path::parent)
            .unwrap_or(Path::new(""))
    }
}

/// The machines `podman machine list` shows: each one's name and whether
/// it runs or is starting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineEntry {
    pub name: String,
    pub running: bool,
    pub starting: bool,
}

/// What to do before a container starts, the machines being `machines`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prepare {
    /// The dagq machine runs.
    Ready,
    /// It is stopped: start it.
    Start,
    /// There is none: create it, then start it.
    Init,
}

/// What to do so the dagq machine runs, or why it cannot: Podman runs one
/// machine at a time, so while another machine (the person's) runs, the
/// dagq machine is not started and the container cannot be used; it is
/// never stopped for dagq (ADR-t2113-1 decision 4).
pub fn prepare(machines: &[MachineEntry]) -> Result<Prepare, String> {
    let ours = machines.iter().find(|machine| machine.name == MACHINE_NAME);
    if ours.is_some_and(|machine| machine.running) {
        return Ok(Prepare::Ready);
    }
    if ours.is_some_and(|machine| machine.starting) {
        return Err(format!(
            "the podman machine {MACHINE_NAME} is starting outside dagq; try again when it runs"
        ));
    }
    let others: Vec<&str> = machines
        .iter()
        .filter(|machine| machine.name != MACHINE_NAME && (machine.running || machine.starting))
        .map(|machine| machine.name.as_str())
        .collect();
    if !others.is_empty() {
        return Err(format!(
            "the podman machine {} runs and Podman runs one machine at a time, so the machine \
{MACHINE_NAME} cannot start; dagq does not stop it: stop it to let containers run",
            others.join(", ")
        ));
    }
    Ok(match ours {
        Some(_) => Prepare::Start,
        None => Prepare::Init,
    })
}

/// Whether the dagq machine is stopped when a user leaves: when it runs
/// and no user is left (ADR-t2113-1 decision 5).
pub fn stops(running: bool, users_left: usize) -> bool {
    running && users_left == 0
}

/// What a user of the dagq machine lives as long as: the user is counted
/// while its owner lives, and is not counted (and forgotten) once it is
/// over, even when it never left.
///
/// A run's worker turns and its verifications are owned by the run in its
/// queue, not by the process that called `ensure`: that process is the
/// run's session wrapper, a long-running supervisor or a person's
/// `integrate`, and a supervisor that outlives a run which ended without
/// leaving would keep the machine running for as long as it runs. A run
/// uses the machine while a turn or a verification of it may still start
/// or run ([`lease_held`]); that is shorter than its relay is kept
/// ([`relay_fate`]), which stays for a run that failed or was interrupted
/// while its task waits for the triage or a person's answer. A test or a
/// smoke that is no run is owned by its own process.
///
/// [`relay_fate`]: super::broker_relay::relay_fate
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseOwner {
    /// The process `pid`: the user ends with it.
    Process { pid: u32 },
    /// The run `run` of the queue whose database is `db`.
    Run { db: PathBuf, run: String },
}

/// What was seen of a user's [`LeaseOwner`] when the users are counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerSeen {
    /// The owner process, and whether it still runs.
    Process { alive: bool },
    /// The owner run as its queue knows it.
    Run(super::broker_relay::RelayRun),
    /// The user's record names no owner this binary reads.
    Unreadable,
}

/// Whether a user whose owner was seen as `seen` still uses the machine.
/// A run does while it is unfinished ([`RunStatus::is_unfinished`]: from
/// its claim to its landing, its turns, its review, its resumes and its
/// verifications among them) and is the latest run of a task in progress;
/// once it failed, was interrupted or landed, or its task ended or moved
/// to another run, it does not, though its task may still wait for the
/// triage or an answer. A run's queue that could not be read keeps its
/// user (a running container is not stopped on a failed read); an
/// unreadable record does not.
///
/// [`RunStatus::is_unfinished`]: super::RunStatus::is_unfinished
pub fn lease_held(seen: OwnerSeen) -> bool {
    use super::broker_relay::RelayRun;
    match seen {
        OwnerSeen::Process { alive } => alive,
        OwnerSeen::Run(RelayRun::Unknown) => true,
        OwnerSeen::Run(RelayRun::Missing) => false,
        OwnerSeen::Run(RelayRun::Found {
            status,
            task_in_progress,
        }) => task_in_progress && status.is_unfinished(),
        OwnerSeen::Unreadable => false,
    }
}

/// Why the volumes the dagq machine was created with (`mounted`, their
/// host paths) do not serve a queue whose directories are `required`:
/// one of them not mounted, or a mount that is not a run, clone or
/// verification directory of a queue directly under `data_root` (such as
/// `$HOME`, which Podman mounts by default, or a directory elsewhere with
/// one of their names). `None` when they serve it. Volumes are set only
/// when a machine is created, so the way out is to create it again.
pub fn mounts_mismatch(
    required: &MachineMounts,
    data_root: &Path,
    mounted: &[PathBuf],
) -> Option<String> {
    let missing: Vec<String> = required
        .dirs()
        .into_iter()
        .filter(|dir| !mounted.iter().any(|mount| mount == dir))
        .map(|dir| dir.display().to_string())
        .collect();
    let unexpected: Vec<String> = mounted
        .iter()
        .filter(|mount| !is_queue_mount(data_root, mount))
        .map(|mount| mount.display().to_string())
        .collect();
    if missing.is_empty() && unexpected.is_empty() {
        return None;
    }
    let mut parts = Vec::new();
    if !missing.is_empty() {
        parts.push(format!("not mounted: {}", missing.join(", ")));
    }
    if !unexpected.is_empty() {
        parts.push(format!(
            "mounted beyond the run, clone and verification directories: {}",
            unexpected.join(", ")
        ));
    }
    Some(format!(
        "the volumes of the podman machine {MACHINE_NAME} do not match what dagq mounts ({}); \
volumes are set only when a machine is created: run `podman machine stop {MACHINE_NAME}` and \
`podman machine rm {MACHINE_NAME}`, and the next container creates it again",
        parts.join("; ")
    ))
}

/// Whether `mount` is `<data_root>/<queue>/<name>`, `<name>` one of
/// [`MOUNT_DIR_NAMES`] and `<queue>` a plain name (not `.` or `..`).
fn is_queue_mount(data_root: &Path, mount: &Path) -> bool {
    let Ok(rest) = mount.strip_prefix(data_root) else {
        return false;
    };
    let parts: Vec<Component<'_>> = rest.components().collect();
    matches!(
        parts.as_slice(),
        [Component::Normal(_), Component::Normal(name)]
            if name.to_str().is_some_and(|name| MOUNT_DIR_NAMES.contains(&name))
    )
}

/// A queue's name among the named volumes of the machine, which every
/// project on the host shares: the queue directory's name (the hash of
/// its repository). Volumes are named after it, so one queue never names
/// another's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueKey(String);

impl QueueKey {
    /// `key` when it is a volume name's part Podman takes (ASCII letters,
    /// digits, `_`, `.` and `-`, a letter or digit first).
    pub fn new(key: &str) -> Result<Self, String> {
        volume_part(key).map(|key| Self(key.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The named volume of the build output of the run (or the
    /// verification) `run` (ADR-t2173-1 decision 2): only its containers
    /// mount it, and it is removed when it ends and at the landing.
    /// `run` is the run's ID, or the verification's, with the same
    /// characters as a key.
    pub fn build_output_volume(&self, run: &str) -> Result<String, String> {
        volume_part(run).map(|run| format!("dagq-{}-build-{run}", self.0))
    }

    /// The named volume of the queue's worker cache (ADR-t2173-1 decision
    /// 3): the workers and resumes of this queue share it, nothing else.
    pub fn worker_cache_volume(&self) -> String {
        format!("dagq-{}-cache", self.0)
    }

    /// The named volume that holds the unix socket of the run `run`
    /// (ADR-t2114-3 decision 2): its relay binds the socket in it, and only
    /// the run's container mounts it beside the relay.
    pub fn socket_volume(&self, run: &str) -> Result<String, String> {
        volume_part(run).map(|run| format!("dagq-{}-socket-{run}", self.0))
    }

    /// The container of the relay of the run `run` (ADR-t2114-3 decision
    /// 2).
    pub fn relay_container(&self, run: &str) -> Result<String, String> {
        volume_part(run).map(|run| format!("dagq-{}-relay-{run}", self.0))
    }

    /// The container a turn of the run `run`'s worker runs in
    /// ([`super::run_container`]): one at a time, so whatever stops the run
    /// removes it by this name.
    pub fn run_container(&self, run: &str) -> Result<String, String> {
        volume_part(run).map(|run| format!("dagq-{}-run-{run}", self.0))
    }

    /// The container the verification `verification` of `integrate` runs
    /// its commands in ([`super::run_container::verification_run_args`]):
    /// one command at a time, so the verification's end removes it by this
    /// name.
    pub fn verification_container(&self, verification: &str) -> Result<String, String> {
        volume_part(verification).map(|id| format!("dagq-{}-verify-{id}", self.0))
    }
}

fn volume_part(part: &str) -> Result<&str, String> {
    let mut chars = part.chars();
    let first = chars.next().is_some_and(|c| c.is_ascii_alphanumeric());
    if first && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')) {
        Ok(part)
    } else {
        Err(format!(
            "{part:?} cannot be part of a podman volume's name (ASCII letters, digits, _, . and - \
only, a letter or digit first)"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, running: bool) -> MachineEntry {
        MachineEntry {
            name: name.to_owned(),
            running,
            starting: false,
        }
    }

    #[test]
    fn without_a_machine_it_is_created_and_a_stopped_one_is_started() {
        assert_eq!(prepare(&[]), Ok(Prepare::Init));
        assert_eq!(
            prepare(&[entry("podman-machine-default", false)]),
            Ok(Prepare::Init)
        );
        assert_eq!(prepare(&[entry(MACHINE_NAME, false)]), Ok(Prepare::Start));
        assert_eq!(prepare(&[entry(MACHINE_NAME, true)]), Ok(Prepare::Ready));
        // The person's machine stopped beside it changes nothing.
        assert_eq!(
            prepare(&[
                entry("podman-machine-default", false),
                entry(MACHINE_NAME, true)
            ]),
            Ok(Prepare::Ready)
        );
    }

    #[test]
    fn while_the_person_s_machine_runs_the_dagq_machine_cannot_be_used() {
        for ours in [None, Some(entry(MACHINE_NAME, false))] {
            let mut machines = vec![entry("podman-machine-default", true)];
            machines.extend(ours);
            let why = prepare(&machines).unwrap_err();
            assert!(why.contains("podman-machine-default runs"), "{why}");
            assert!(why.contains("does not stop it"), "{why}");
        }
        let starting = MachineEntry {
            starting: true,
            ..entry("mine", false)
        };
        assert!(prepare(&[starting]).is_err());
        let ours_starting = MachineEntry {
            starting: true,
            ..entry(MACHINE_NAME, false)
        };
        assert!(prepare(&[ours_starting]).unwrap_err().contains("starting"));
    }

    #[test]
    fn the_machine_stops_when_it_runs_and_no_user_is_left() {
        assert!(stops(true, 0));
        assert!(!stops(true, 1));
        assert!(!stops(false, 0));
    }

    #[test]
    fn a_queue_mounts_its_runs_clones_and_verification_directories() {
        let mounts = MachineMounts::of_queue(Path::new("/data/dagq/abc"));
        assert_eq!(
            mounts.dirs(),
            [
                Path::new("/data/dagq/abc/runs"),
                Path::new("/data/dagq/abc/clones"),
                Path::new("/data/dagq/abc/verification"),
            ]
        );
    }

    #[test]
    fn mounts_that_lack_the_queue_s_or_hold_more_do_not_match() {
        let required = MachineMounts::of_queue(Path::new("/q/a"));
        let mut mounted: Vec<PathBuf> = required.dirs().map(Path::to_path_buf).to_vec();
        let root = Path::new("/q");
        assert_eq!(mounts_mismatch(&required, root, &mounted), None);
        // Another queue's directories beside them serve it too.
        mounted.extend(
            MachineMounts::of_queue(Path::new("/q/b"))
                .dirs()
                .map(Path::to_path_buf),
        );
        assert_eq!(mounts_mismatch(&required, root, &mounted), None);

        let why = mounts_mismatch(&required, root, &mounted[1..]).unwrap();
        assert!(why.contains("not mounted: /q/a/runs"), "{why}");
        assert!(why.contains("podman machine rm dagq"), "{why}");

        mounted.push(PathBuf::from("/Users/me"));
        let why = mounts_mismatch(&required, root, &mounted).unwrap();
        assert!(why.contains("beyond the run, clone and verification directories: /Users/me"));
        assert!(!why.contains("not mounted"), "{why}");
    }

    #[test]
    fn a_directory_named_like_a_queue_s_but_not_under_the_data_root_does_not_match() {
        let required = MachineMounts::of_queue(Path::new("/data/dagq/a"));
        let root = required.data_root();
        assert_eq!(root, Path::new("/data/dagq"));
        let served: Vec<PathBuf> = required.dirs().map(Path::to_path_buf).to_vec();
        for stray in [
            "/Users/me/runs",
            "/data/runs",
            "/data/dagq/runs",
            "/data/dagq/a/b/clones",
            "/data/dagq/../me/verification",
            "/data/dagq/a/cache",
        ] {
            let mut mounted = served.clone();
            mounted.push(PathBuf::from(stray));
            let why = mounts_mismatch(&required, root, &mounted).unwrap();
            assert!(
                why.contains(&format!(
                    "beyond the run, clone and verification directories: {stray}"
                )),
                "{why}"
            );
            assert!(!why.contains("not mounted"), "{why}");
        }
    }

    #[test]
    fn volume_names_part_queues_and_runs() {
        let a = QueueKey::new("77067154921b9014").unwrap();
        let b = QueueKey::new("0123456789abcdef").unwrap();
        assert_eq!(
            a.build_output_volume("r1").unwrap(),
            "dagq-77067154921b9014-build-r1"
        );
        assert_ne!(a.build_output_volume("r1"), a.build_output_volume("r2"));
        assert_ne!(a.build_output_volume("r1"), b.build_output_volume("r1"));
        assert!(a.build_output_volume("../x").is_err());
        assert_eq!(a.worker_cache_volume(), "dagq-77067154921b9014-cache");
        assert_ne!(a.worker_cache_volume(), b.worker_cache_volume());
        for bad in ["", "-a", "a/b", "a b", "ä"] {
            assert!(QueueKey::new(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_default_resources_are_the_documented_modest_ones() {
        assert_eq!(
            MachineResources::default(),
            MachineResources {
                cpus: 2,
                memory_mib: 4096,
                disk_gib: 60
            }
        );
    }

    #[test]
    fn a_run_s_user_lasts_as_long_as_the_run_not_as_the_process_that_joined() {
        use super::super::RunStatus;
        use super::super::broker_relay::RelayRun;
        let run = |status, task_in_progress| {
            lease_held(OwnerSeen::Run(RelayRun::Found {
                status,
                task_in_progress,
            }))
        };
        // Still at work, at its review, waiting for a resume or at its
        // landing's verification: counted.
        assert!(run(RunStatus::Running, true));
        assert!(run(RunStatus::Validating, true));
        assert!(run(RunStatus::NeedsSession, true));
        assert!(run(RunStatus::AwaitingIntegration, true));
        assert!(run(RunStatus::Integrating, true));
        // Over, whoever still runs: not counted, even while its task waits
        // in progress for the triage or an answer.
        assert!(!run(RunStatus::Failed, true));
        assert!(!run(RunStatus::Interrupted, true));
        assert!(!run(RunStatus::Integrated, true));
        assert!(!run(RunStatus::Running, false));
        assert!(!lease_held(OwnerSeen::Run(RelayRun::Missing)));
        // A queue that could not be read keeps it.
        assert!(lease_held(OwnerSeen::Run(RelayRun::Unknown)));
        // A test's or a smoke's process.
        assert!(lease_held(OwnerSeen::Process { alive: true }));
        assert!(!lease_held(OwnerSeen::Process { alive: false }));
        assert!(!lease_held(OwnerSeen::Unreadable));
    }
}
