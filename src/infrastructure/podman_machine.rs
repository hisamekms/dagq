//! The dagq podman machine on the host (ADR-t2113-1 decisions 4 to 6,
//! ADR-t2173-1): the podman commands ([`PodmanCli`]), the host-wide lock
//! and the users ([`HostMachineUsers`]), the named volumes of a queue
//! ([`QueueVolumes`]), the directories a new machine mounts
//! ([`host_volumes`]) and the `[machine]` table of the host-wide
//! `host.toml` ([`load_machine_resources`]).
//!
//! Every command names the dagq machine: `podman machine <verb> dagq`, and
//! `podman --connection dagq volume ...` inside it; `podman machine list`
//! reads every machine, to see whether the person's runs. Creating and
//! starting a machine may make its connection the default on some Podman
//! versions, so the default connection read before is set back after.
//! The volumes a machine was created with are read from its configuration
//! file (`<ConfigDir>/<name>.json` of `podman machine inspect`), the only
//! place Podman keeps them.
//!
//! Everything host-wide lives in `<data home>/dagq/machine/`: `lock`
//! (held with `flock` while the machine is started or stopped, and while a
//! container's memory is reserved), `users/<user>` (the user's
//! [`LeaseOwner`] as JSON), and the memory the containers of every queue
//! reserved and wait for (ADR-t3163-1): `reservations/<container>` and
//! `waiting/<container>`, each the container's [`MemoryAsk`] as JSON. The
//! machine's memory and its containers are read from it
//! ([`MachineMemory`] of [`PodmanCliWithin`]).

use std::any::Any;
use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use super::location::DB_FILE_NAME;
use super::run_env::{parse_positive, strip_comment};
use crate::application::podman_machine::{
    Machine, MachineMemory, MachineReservations, MachineUsers, MachineVolumes, MemoryMachine,
    MemoryRoom, MemoryStep, PodmanMachine, RoomLook,
};
use crate::domain::broker_relay::RelayRun;
use crate::domain::machine_memory::{MachineContainer, MemoryAsk};
use crate::domain::podman_machine::{
    LeaseOwner, MACHINE_NAME, MachineEntry, MachineMounts, MachineResources, OwnerSeen, QueueKey,
    lease_held,
};

/// The directory beside the queues in the data home that holds the
/// machine's host-wide lock and users.
pub const MACHINE_DIR_NAME: &str = "machine";

/// The podman commands on the dagq machine, run with `program`.
#[derive(Debug, Clone)]
pub struct PodmanCli {
    pub program: PathBuf,
}

impl Default for PodmanCli {
    fn default() -> Self {
        Self {
            program: PathBuf::from("podman"),
        }
    }
}

impl PodmanCli {
    fn output(&self, args: &[OsString]) -> Result<Output> {
        Command::new(&self.program)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .with_context(|| format!("run {}", self.program.display()))
    }

    /// `args` given at most `limit` when there is one: an error when they
    /// did not end in time and were killed.
    fn output_by(&self, args: &[OsString], limit: Option<Duration>) -> Result<Output> {
        let Some(limit) = limit else {
            return self.output(args);
        };
        match self.output_within(args, limit)? {
            Some(output) => Ok(output),
            None => bail!(
                "{} {} did not end within {limit:?}: it was stopped",
                self.program.display(),
                shown(args)
            ),
        }
    }

    /// `args` run to success, given at most `limit` when there is one;
    /// their stdout.
    fn run_by(&self, args: &[&str], limit: Option<Duration>) -> Result<String> {
        let args: Vec<OsString> = args.iter().map(OsString::from).collect();
        self.checked_by(&args, limit)
    }

    fn checked(&self, args: &[OsString]) -> Result<String> {
        self.checked_by(args, None)
    }

    fn checked_by(&self, args: &[OsString], limit: Option<Duration>) -> Result<String> {
        let output = self.output_by(args, limit)?;
        if !output.status.success() {
            bail!(
                "{} {} exited with {}: {}",
                self.program.display(),
                shown(args),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// The connection that is the default now, if any.
    fn default_connection(&self, limit: Option<Duration>) -> Result<Option<String>> {
        let text = self.run_by(&["system", "connection", "list", "--format", "json"], limit)?;
        parse_default_connection(&text)
    }

    /// Run `act`, then set the default connection back to the one before
    /// it, if `act` changed it. A host with no default before may keep the
    /// dagq machine's connection as its default: Podman cannot unset it.
    fn keeping_default(
        &self,
        limit: Option<Duration>,
        act: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        let before = self.default_connection(limit)?;
        let acted = act();
        let restore = || -> Result<()> {
            if let Some(before) = before
                && self.default_connection(limit)?.as_deref() != Some(before.as_str())
            {
                self.run_by(&["system", "connection", "default", &before], limit)?;
            }
            Ok(())
        };
        let restored = restore();
        // What `act` failed with is told before a failure to set it back.
        acted.and(restored)
    }

    /// `podman --connection dagq <args>` run to success, inside the dagq
    /// machine; its stdout.
    pub(super) fn on_machine(&self, args: &[OsString]) -> Result<String> {
        let mut all: Vec<OsString> = vec!["--connection".into(), MACHINE_NAME.into()];
        all.extend(args.iter().cloned());
        self.checked(&all)
    }

    /// `podman --connection dagq <args>` given at most `limit`: its output
    /// whatever its exit, or `None` when it did not end in time and was
    /// killed (or its output was not all read by then). A podman whose
    /// machine hangs does not hold its caller.
    pub(crate) fn inside_within(&self, args: &[String], limit: Duration) -> Result<Option<Output>> {
        self.output_within(&connection_args(args), limit)
    }

    /// `args` given at most `limit`: their output whatever their exit, or
    /// `None` when they did not end in time and were killed (or their
    /// output was not all read by then).
    fn output_within(&self, args: &[OsString], limit: Duration) -> Result<Option<Output>> {
        let mut child = Command::new(&self.program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("run {}", self.program.display()))?;
        // The pipes are read on their own threads, so a podman that writes
        // much does not block on them while it is waited for; what they
        // read is waited for until the same deadline, so a process podman
        // left behind holding them does not hold the caller either.
        let read = |pipe: Option<Box<dyn Read + Send>>| {
            let (sent, received) = std::sync::mpsc::channel();
            thread::spawn(move || {
                let mut bytes = Vec::new();
                if let Some(mut pipe) = pipe {
                    let _ = pipe.read_to_end(&mut bytes);
                }
                let _ = sent.send(bytes);
            });
            received
        };
        let stdout = read(child.stdout.take().map(|pipe| Box::new(pipe) as _));
        let stderr = read(child.stderr.take().map(|pipe| Box::new(pipe) as _));
        let deadline = Instant::now() + limit;
        let status = loop {
            let ended = child.try_wait();
            if let Ok(Some(status)) = ended {
                break status;
            }
            if ended.is_err() || Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                ended.with_context(|| format!("wait for {}", self.program.display()))?;
                return Ok(None);
            }
            thread::sleep(Duration::from_millis(20));
        };
        let collect = |received: std::sync::mpsc::Receiver<Vec<u8>>| {
            received.recv_timeout(deadline.saturating_duration_since(Instant::now()))
        };
        match (collect(stdout), collect(stderr)) {
            (Ok(stdout), Ok(stderr)) => Ok(Some(Output {
                status,
                stdout,
                stderr,
            })),
            _ => Ok(None),
        }
    }

    /// `podman --connection dagq volume <args>`.
    fn volume(&self, args: &[&str]) -> Result<Output> {
        let mut all: Vec<OsString> = ["--connection", MACHINE_NAME, "volume"]
            .iter()
            .map(OsString::from)
            .collect();
        all.extend(args.iter().map(OsString::from));
        self.output(&all)
    }
}

impl PodmanCli {
    fn machines_by(&self, limit: Option<Duration>) -> Result<Vec<MachineEntry>> {
        parse_machine_list(&self.run_by(&["machine", "list", "--format", "json"], limit)?)
    }

    fn mounts_by(&self, limit: Option<Duration>) -> Result<Vec<PathBuf>> {
        let inspect = self.run_by(&["machine", "inspect", MACHINE_NAME], limit)?;
        let file = config_file(&inspect)?;
        let text = fs::read_to_string(&file).with_context(|| format!("read {}", file.display()))?;
        parse_config_mounts(&text).with_context(|| format!("read {}", file.display()))
    }

    fn init_by(
        &self,
        resources: &MachineResources,
        volumes: &[PathBuf],
        limit: Option<Duration>,
    ) -> Result<()> {
        // A volume's source must be there when the machine starts.
        for dir in volumes {
            fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        let args = init_args(resources, volumes);
        self.keeping_default(limit, || self.checked_by(&args, limit).map(drop))
    }

    fn start_by(&self, limit: Option<Duration>) -> Result<()> {
        self.keeping_default(limit, || {
            self.run_by(&["machine", "start", MACHINE_NAME], limit)
                .map(drop)
        })
    }

    fn stop_by(&self, limit: Option<Duration>) -> Result<()> {
        self.run_by(&["machine", "stop", MACHINE_NAME], limit)
            .map(drop)
    }
}

impl PodmanMachine for PodmanCli {
    fn machines(&self) -> Result<Vec<MachineEntry>> {
        self.machines_by(None)
    }

    fn mounts(&self) -> Result<Vec<PathBuf>> {
        self.mounts_by(None)
    }

    fn init(&self, resources: &MachineResources, volumes: &[PathBuf]) -> Result<()> {
        self.init_by(resources, volumes, None)
    }

    fn start(&self) -> Result<()> {
        self.start_by(None)
    }

    fn stop(&self) -> Result<()> {
        self.stop_by(None)
    }
}

/// How long each podman command of a machine's start again from the
/// relays' watch may take ([`PodmanCliWithin`]): 5 min. A machine's start
/// boots its VM and waits for its gvproxy and ssh, which takes tens of
/// seconds when all is well and longer on a loaded host, and its stop
/// waits for the VM to shut down; a relay's command
/// ([`super::broker_relay::RELAY_TIMEOUT`]) ends far sooner, so the
/// machine's is its own and longer. A command still running then is
/// killed and the start again fails, so a machine that does not come back
/// holds the supervisor's loop for at most this per command (the list, the
/// stop, the start and the default connection's reads and set back).
pub const MACHINE_TIMEOUT: Duration = Duration::from_secs(300);

/// The dagq machine's commands of `podman`, each given at most `limit`
/// ([`MACHINE_TIMEOUT`]): one that does not end in time is killed and is
/// an error.
#[derive(Debug, Clone)]
pub struct PodmanCliWithin {
    pub podman: PodmanCli,
    pub limit: Duration,
}

impl PodmanMachine for PodmanCliWithin {
    fn machines(&self) -> Result<Vec<MachineEntry>> {
        self.podman.machines_by(Some(self.limit))
    }

    fn mounts(&self) -> Result<Vec<PathBuf>> {
        self.podman.mounts_by(Some(self.limit))
    }

    fn init(&self, resources: &MachineResources, volumes: &[PathBuf]) -> Result<()> {
        self.podman.init_by(resources, volumes, Some(self.limit))
    }

    fn start(&self) -> Result<()> {
        self.podman.start_by(Some(self.limit))
    }

    fn stop(&self) -> Result<()> {
        self.podman.stop_by(Some(self.limit))
    }
}

/// How long each podman command that reads the machine's memory and its
/// containers may take ([`MachineMemory`] of [`PodmanCliWithin`]): 60 s,
/// as a relay's look. They run under the machine's host-wide lock while a
/// container's memory is reserved, so a machine that hangs holds the lock
/// for at most this per command.
pub const MEMORY_LOOK_TIMEOUT: Duration = Duration::from_secs(60);

impl PodmanCliWithin {
    fn inside(&self, args: &[&str]) -> Result<String> {
        let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
        let output = self
            .podman
            .inside_within(&args, self.limit)?
            .with_context(|| {
                format!(
                    "podman --connection {MACHINE_NAME} {} did not end within {:?}: it was stopped",
                    args.join(" "),
                    self.limit
                )
            })?;
        if !output.status.success() {
            bail!(
                "podman --connection {MACHINE_NAME} {} exited with {}: {}",
                args.join(" "),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

impl MachineMemory for PodmanCliWithin {
    fn memory_mib(&self) -> Result<u64> {
        let inspect = self
            .podman
            .run_by(&["machine", "inspect", MACHINE_NAME], Some(self.limit))?;
        parse_machine_memory(&inspect)
    }

    /// The containers labelled with a queue, listed and then inspected: one
    /// that ended between the two fails the read, which is tried again.
    fn containers(&self) -> Result<Vec<MachineContainer>> {
        let label = format!("label={}", crate::domain::broker_relay::QUEUE_LABEL);
        let listed = self.inside(&["ps", "--quiet", "--no-trunc", "--filter", &label])?;
        let ids: Vec<&str> = listed.split_whitespace().collect();
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut args = vec!["container", "inspect"];
        args.extend(ids);
        parse_container_inspect(&self.inside(&args)?)
    }
}

/// The memory of `podman machine inspect`'s machine, in MiB
/// (`Resources.Memory`).
pub fn parse_machine_memory(inspect: &str) -> Result<u64> {
    let value: serde_json::Value =
        serde_json::from_str(inspect).context("read podman machine inspect")?;
    value
        .get(0)
        .and_then(|machine| machine.pointer("/Resources/Memory"))
        .and_then(serde_json::Value::as_u64)
        .context("podman machine inspect names no Resources.Memory")
}

/// The containers of `podman container inspect`: each one's name, its
/// memory limit (`HostConfig.Memory` in bytes, none when 0) and whether it
/// is a relay (labelled with [`crate::domain::broker_relay::RUN_LABEL`]).
pub fn parse_container_inspect(text: &str) -> Result<Vec<MachineContainer>> {
    let value: serde_json::Value =
        serde_json::from_str(text).context("read podman container inspect")?;
    let Some(containers) = value.as_array() else {
        bail!("podman container inspect gave no list");
    };
    containers
        .iter()
        .map(|container| {
            let name = container
                .get("Name")
                .and_then(serde_json::Value::as_str)
                .context("a container without a Name")?
                .trim_start_matches('/')
                .to_owned();
            let memory = container
                .pointer("/HostConfig/Memory")
                .and_then(serde_json::Value::as_u64)
                .filter(|bytes| *bytes > 0)
                .map(|bytes| bytes.div_ceil(1024 * 1024));
            let relay = container
                .pointer("/Config/Labels")
                .and_then(|labels| labels.get(crate::domain::broker_relay::RUN_LABEL))
                .is_some();
            Ok(MachineContainer {
                name,
                memory_mib: memory,
                relay,
            })
        })
        .collect()
}

/// `--connection dagq` and `args`.
fn connection_args(args: &[String]) -> Vec<OsString> {
    let mut all: Vec<OsString> = ["--connection", MACHINE_NAME]
        .iter()
        .map(OsString::from)
        .collect();
    all.extend(args.iter().map(OsString::from));
    all
}

/// `args` as a command line in a message.
fn shown(args: &[OsString]) -> String {
    args.iter()
        .map(|arg| arg.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ")
}

/// `podman machine init` of the dagq machine: its resources, and each of
/// `volumes` at the same path inside it. Naming volumes replaces Podman's
/// default ones (`$HOME`).
pub fn init_args(resources: &MachineResources, volumes: &[PathBuf]) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        "machine".into(),
        "init".into(),
        "--cpus".into(),
        resources.cpus.to_string().into(),
        "--memory".into(),
        resources.memory_mib.to_string().into(),
        "--disk-size".into(),
        resources.disk_gib.to_string().into(),
    ];
    for dir in volumes {
        let mut volume = dir.as_os_str().to_owned();
        volume.push(":");
        volume.push(dir.as_os_str());
        args.push("--volume".into());
        args.push(volume);
    }
    args.push(MACHINE_NAME.into());
    args
}

#[derive(Deserialize)]
struct ListedMachine {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Running", default)]
    running: bool,
    #[serde(rename = "Starting", default)]
    starting: bool,
}

/// The machines of `podman machine list --format json`.
pub fn parse_machine_list(text: &str) -> Result<Vec<MachineEntry>> {
    let listed: Option<Vec<ListedMachine>> =
        serde_json::from_str(text).context("read podman machine list")?;
    Ok(listed
        .unwrap_or_default()
        .into_iter()
        .map(|machine| MachineEntry {
            // The default machine's name is marked with `*` in a table.
            name: machine.name.trim_end_matches('*').to_owned(),
            running: machine.running,
            starting: machine.starting,
        })
        .collect())
}

#[derive(Deserialize)]
struct Connection {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Default", default)]
    default: bool,
}

/// The default connection of `podman system connection list --format json`.
pub fn parse_default_connection(text: &str) -> Result<Option<String>> {
    let connections: Option<Vec<Connection>> =
        serde_json::from_str(text).context("read podman system connection list")?;
    Ok(connections
        .unwrap_or_default()
        .into_iter()
        .find(|connection| connection.default)
        .map(|connection| connection.name))
}

/// The configuration file of the dagq machine, from `podman machine
/// inspect`'s `ConfigDir`.
pub fn config_file(inspect: &str) -> Result<PathBuf> {
    let value: serde_json::Value =
        serde_json::from_str(inspect).context("read podman machine inspect")?;
    let dir = value
        .get(0)
        .and_then(|machine| machine.pointer("/ConfigDir/Path"))
        .and_then(serde_json::Value::as_str)
        .context("podman machine inspect names no ConfigDir")?;
    Ok(Path::new(dir).join(format!("{MACHINE_NAME}.json")))
}

/// The host paths of the volumes in a machine's configuration file.
pub fn parse_config_mounts(text: &str) -> Result<Vec<PathBuf>> {
    let value: serde_json::Value = serde_json::from_str(text)?;
    let Some(mounts) = value.get("Mounts").filter(|mounts| !mounts.is_null()) else {
        return Ok(Vec::new());
    };
    mounts
        .as_array()
        .context("Mounts is not a list")?
        .iter()
        .map(|mount| {
            mount
                .get("Source")
                .and_then(serde_json::Value::as_str)
                .map(PathBuf::from)
                .context("a mount without a Source")
        })
        .collect()
}

/// The directories a new dagq machine mounts: those of every queue under
/// `data_root` (`<data home>/dagq`, each directory with a queue database)
/// and of `queue_dir`, so the queues of the host when it is created share
/// it. A queue made after that is not mounted, and the machine is created
/// again for it ([`crate::domain::podman_machine::mounts_mismatch`]).
pub fn host_volumes(data_root: &Path, queue_dir: &Path) -> Vec<PathBuf> {
    let mut queues = vec![queue_dir.to_path_buf()];
    if let Ok(entries) = fs::read_dir(data_root) {
        queues.extend(
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|dir| dir.join(DB_FILE_NAME).is_file()),
        );
    }
    let mut volumes: Vec<PathBuf> = queues
        .iter()
        .flat_map(|queue| MachineMounts::of_queue(queue).dirs().map(Path::to_path_buf))
        .collect();
    volumes.sort();
    volumes.dedup();
    volumes
}

/// The host-wide lock and users of the machine, in `dir`
/// (`<data home>/dagq/machine`).
#[derive(Clone)]
pub struct HostMachineUsers {
    pub dir: PathBuf,
    /// How the runs that own users are read when the process opens no
    /// queue ([`OwnerRuns`]); on each owner's queue's DB without.
    pub runs: Option<std::sync::Arc<dyn OwnerRuns>>,
}

impl std::fmt::Debug for HostMachineUsers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostMachineUsers")
            .field("dir", &self.dir)
            .field("runs", &self.runs.is_some())
            .finish()
    }
}

/// How a process that opens no queue (the supervisor, a run's session
/// wrapper, a person's `integrate` through the queue service) reads the runs that own users of
/// the machine: the runs of the queue it is a client of, through that
/// queue's service. A run of a queue it does not reach is unknown, and its
/// user stays counted and kept ([`lease_held`]), unless that queue's
/// database is gone (no such run): no queue's DB is opened.
pub trait OwnerRuns: Send + Sync {
    /// The run `run` of the queue at `db` as its service answers
    /// ([`relay_run_in`]); `None` for a queue this reader does not reach.
    fn run(&self, db: &Path, run: &str) -> Option<RelayRun>;
}

impl HostMachineUsers {
    fn users(&self) -> PathBuf {
        self.dir.join("users")
    }

    fn user_file(&self, user: &str) -> Result<PathBuf> {
        if user.is_empty() || user.starts_with('.') || user.contains(['/', '\0']) {
            bail!("{user:?} cannot name a user of the podman machine");
        }
        Ok(self.users().join(user))
    }
}

impl MachineUsers for HostMachineUsers {
    fn lock(&self) -> Result<Box<dyn Any>> {
        fs::create_dir_all(&self.dir).with_context(|| format!("create {}", self.dir.display()))?;
        let path = self.dir.join("lock");
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        // SAFETY: flock on a descriptor this function owns; it blocks until
        // the lock is free and is released when the file closes.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("lock {}", path.display()));
        }
        Ok(Box::new(file))
    }

    fn try_lock(&self) -> Result<Option<Box<dyn Any>>> {
        fs::create_dir_all(&self.dir).with_context(|| format!("create {}", self.dir.display()))?;
        let path = self.dir.join("lock");
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        // SAFETY: flock on a descriptor this function owns; it does not
        // block, and is released when the file closes.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
                return Ok(None);
            }
            return Err(error).with_context(|| format!("lock {}", path.display()));
        }
        Ok(Some(Box::new(file)))
    }

    fn join(&self, user: &str, owner: &LeaseOwner) -> Result<()> {
        let file = self.user_file(user)?;
        fs::create_dir_all(self.users())?;
        fs::write(&file, serde_json::to_vec(owner)?)
            .with_context(|| format!("write {}", file.display()))
    }

    fn leave(&self, user: &str) -> Result<()> {
        let file = self.user_file(user)?;
        match fs::remove_file(&file) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                Err(error).with_context(|| format!("remove {}", file.display()))
            }
            _ => Ok(()),
        }
    }

    fn count(&self) -> Result<usize> {
        let entries = match fs::read_dir(self.users()) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(error).context("read the machine's users"),
        };
        let mut count = 0;
        let mut queues = Queues::new();
        for entry in entries.flatten() {
            let seen = match fs::read_to_string(entry.path()) {
                Ok(text) => owner_seen(&text, &mut queues, self.runs.as_deref()),
                // Left between the listing and the read.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => OwnerSeen::Unreadable,
            };
            if lease_held(seen) {
                count += 1;
            } else {
                // Its owner is over, though it never left.
                let _ = fs::remove_file(entry.path());
            }
        }
        Ok(count)
    }
}

impl HostMachineUsers {
    fn asks_dir(&self, kind: &str) -> PathBuf {
        self.dir.join(kind)
    }

    fn ask_file(&self, kind: &str, container: &str) -> Result<PathBuf> {
        if container.is_empty() || container.starts_with('.') || container.contains(['/', '\0']) {
            bail!("{container:?} cannot name a container of the podman machine");
        }
        Ok(self.asks_dir(kind).join(container))
    }

    /// The asks recorded in `kind`, skipping one being written (`.tmp`) and
    /// one that cannot be read: a read may be a look without the lock, so
    /// it removes nothing.
    fn asks(&self, kind: &str) -> Result<Vec<MemoryAsk>> {
        let dir = self.asks_dir(kind);
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error).with_context(|| format!("read {}", dir.display())),
        };
        let mut asks = Vec::new();
        for entry in entries.flatten() {
            if entry.path().extension().is_some_and(|ext| ext == "tmp") {
                continue;
            }
            match fs::read(entry.path()) {
                Ok(bytes) => {
                    if let Ok(ask) = serde_json::from_slice::<MemoryAsk>(&bytes) {
                        asks.push(ask);
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error).with_context(|| format!("read {}", entry.path().display()));
                }
            }
        }
        asks.sort_by(|a, b| (a.seq, a.since, &a.container).cmp(&(b.seq, b.since, &b.container)));
        Ok(asks)
    }

    fn write_ask(&self, kind: &str, ask: &MemoryAsk) -> Result<()> {
        let file = self.ask_file(kind, &ask.container)?;
        fs::create_dir_all(self.asks_dir(kind))?;
        let tmp = file.with_extension("tmp");
        fs::write(&tmp, serde_json::to_vec(ask)?)
            .with_context(|| format!("write {}", tmp.display()))?;
        fs::rename(&tmp, &file).with_context(|| format!("write {}", file.display()))
    }

    fn forget(&self, kind: &str, container: &str) -> Result<()> {
        let file = self.ask_file(kind, container)?;
        match fs::remove_file(&file) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                Err(error).with_context(|| format!("remove {}", file.display()))
            }
            _ => Ok(()),
        }
    }
}

/// The directories of [`HostMachineUsers`] the reservations and the waits
/// of the machine's memory are in.
const RESERVATIONS: &str = "reservations";
const WAITING: &str = "waiting";

impl MachineReservations for HostMachineUsers {
    fn reservations(&self) -> Result<Vec<MemoryAsk>> {
        self.asks(RESERVATIONS)
    }

    fn waits(&self) -> Result<Vec<MemoryAsk>> {
        self.asks(WAITING)
    }

    fn reserve(&self, ask: &MemoryAsk) -> Result<()> {
        self.write_ask(RESERVATIONS, ask)?;
        self.forget(WAITING, &ask.container)
    }

    fn unreserve(&self, container: &str) -> Result<()> {
        self.forget(RESERVATIONS, container)
    }

    fn wait(&self, ask: &MemoryAsk) -> Result<()> {
        if self.ask_file(WAITING, &ask.container)?.exists() {
            return Ok(());
        }
        self.write_ask(WAITING, ask)
    }

    fn unwait(&self, container: &str) -> Result<()> {
        self.forget(WAITING, container)
    }

    fn owner_held(&self, owner: &LeaseOwner) -> bool {
        lease_held(seen_of(owner, &mut Queues::new(), self.runs.as_deref()))
    }

    fn process_alive(&self, pid: u32) -> bool {
        super::adapters::process_alive(pid)
    }
}

/// The queues the users of one count name, each opened once: `None` when
/// it could not be read (why is logged), and absent while not opened.
type Queues = std::collections::HashMap<PathBuf, Option<super::sqlite::SqliteQueue>>;

/// What is seen now of the owner a user's record `text` names: a bare pid
/// is a process (the record of a binary from before the owners).
fn owner_seen(text: &str, queues: &mut Queues, runs: Option<&dyn OwnerRuns>) -> OwnerSeen {
    let owner = serde_json::from_str(text).ok().or_else(|| {
        text.trim()
            .parse()
            .ok()
            .map(|pid| LeaseOwner::Process { pid })
    });
    match owner {
        Some(owner) => seen_of(&owner, queues, runs),
        None => OwnerSeen::Unreadable,
    }
}

/// What is seen now of `owner`: a run through `runs` when given (a queue
/// it does not reach unknown, unless its database is gone), else on its
/// queue's DB.
fn seen_of(owner: &LeaseOwner, queues: &mut Queues, runs: Option<&dyn OwnerRuns>) -> OwnerSeen {
    match owner {
        LeaseOwner::Process { pid } => OwnerSeen::Process {
            alive: super::adapters::process_alive(*pid),
        },
        LeaseOwner::Run { db, run } => OwnerSeen::Run(match runs {
            Some(runs) => runs.run(db, run).unwrap_or_else(|| match db.try_exists() {
                Ok(false) => RelayRun::Missing,
                _ => RelayRun::Unknown,
            }),
            None => run_in_queue(db, run, queues),
        }),
    }
}

/// The run `run` as the queue at `db` knows it, read without changing it:
/// a queue whose database is gone has no such run, and one that cannot
/// be looked at (a directory it cannot enter) is unknown, not gone.
fn run_in_queue(db: &Path, run: &str, queues: &mut Queues) -> RelayRun {
    match db.try_exists() {
        Ok(true) => {}
        Ok(false) => return RelayRun::Missing,
        Err(error) => {
            tracing::warn!(db = %db.display(), error = %format_args!("{error:#}"), "the queue of a user of the podman machine could not be looked at: {error:#}");
            return RelayRun::Unknown;
        }
    }
    let opened = queues.entry(db.to_path_buf()).or_insert_with(|| {
        super::sqlite::SqliteQueue::open_read_only(db)
            .inspect_err(|error| {
                tracing::warn!(db = %db.display(), error = %format_args!("{error:#}"), "the queue of a user of the podman machine could not be read: {error:#}");
            })
            .ok()
    });
    let Some(queue) = opened else {
        return RelayRun::Unknown;
    };
    relay_run_in(queue, run)
}

pub use crate::application::broker_relay::relay_run_in;

/// The named volumes of one queue inside the dagq machine.
#[derive(Debug, Clone)]
pub struct QueueVolumes {
    pub podman: PodmanCli,
    pub queue: QueueKey,
}

impl QueueVolumes {
    /// The volumes of the queue whose directory is `queue_dir`.
    pub fn of_queue(podman: PodmanCli, queue_dir: &Path) -> Result<Self> {
        let name = queue_dir
            .file_name()
            .and_then(|name| name.to_str())
            .with_context(|| format!("{} names no queue", queue_dir.display()))?;
        Ok(Self {
            podman,
            queue: QueueKey::new(name).map_err(anyhow::Error::msg)?,
        })
    }

    fn exists(&self, name: &str) -> Result<bool> {
        let output = self.podman.volume(&["exists", name])?;
        match output.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => bail!(
                "podman volume exists {name} exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        }
    }

    /// `--ignore` leaves a volume that is there, so runs that create the
    /// queue's worker cache at once do not fail one another.
    fn create(&self, name: &str) -> Result<()> {
        let label = format!("dagq.queue={}", self.queue.as_str());
        succeeded(
            self.podman
                .volume(&["create", "--ignore", "--label", &label, name])?,
            "create",
            name,
        )
    }

    fn remove(&self, name: &str) -> Result<()> {
        if !self.exists(name)? {
            return Ok(());
        }
        succeeded(self.podman.volume(&["rm", name])?, "rm", name)
    }

    fn build_output(&self, run: &str) -> Result<String> {
        self.queue
            .build_output_volume(run)
            .map_err(anyhow::Error::msg)
    }
}

fn succeeded(output: Output, verb: &str, name: &str) -> Result<()> {
    if output.status.success() {
        return Ok(());
    }
    bail!(
        "podman volume {verb} {name} exited with {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    )
}

impl MachineVolumes for QueueVolumes {
    fn create_build_output(&self, run: &str) -> Result<()> {
        self.create(&self.build_output(run)?)
    }

    fn remove_build_output(&self, run: &str) -> Result<()> {
        self.remove(&self.build_output(run)?)
    }

    fn build_output_exists(&self, run: &str) -> Result<bool> {
        self.exists(&self.build_output(run)?)
    }

    fn create_worker_cache(&self, dir: &str) -> Result<()> {
        self.create(&self.queue.worker_cache_volume_of(dir))
    }

    fn remove_worker_cache(&self, dir: &str) -> Result<()> {
        self.remove(&self.queue.worker_cache_volume_of(dir))
    }

    fn worker_cache_exists(&self, dir: &str) -> Result<bool> {
        self.exists(&self.queue.worker_cache_volume_of(dir))
    }
}

/// The dagq machine as one queue uses it: what [`ensure`] and [`release`]
/// act with.
///
/// [`ensure`]: crate::application::podman_machine::ensure
/// [`release`]: crate::application::podman_machine::release
#[derive(Debug, Clone)]
pub struct QueueMachine {
    pub podman: PodmanCli,
    pub users: HostMachineUsers,
    pub resources: MachineResources,
    pub required: MachineMounts,
    pub volumes: Vec<PathBuf>,
}

impl QueueMachine {
    /// The machine for the queue whose directory is `queue_dir`, the
    /// queues being under `data_root` (`<data home>/dagq`) and the
    /// resources those of `host_wide` (the host-wide `host.toml`).
    pub fn new(
        podman: PodmanCli,
        data_root: &Path,
        queue_dir: &Path,
        host_wide: Option<&Path>,
    ) -> Self {
        Self {
            podman,
            users: HostMachineUsers {
                dir: data_root.join(MACHINE_DIR_NAME),
                runs: None,
            },
            resources: load_machine_resources(host_wide).0,
            required: MachineMounts::of_queue(queue_dir),
            volumes: host_volumes(data_root, queue_dir),
        }
    }

    pub fn machine(&self) -> Machine<'_> {
        Machine {
            podman: &self.podman,
            users: &self.users,
            resources: self.resources,
            required: self.required.clone(),
            volumes: self.volumes.clone(),
        }
    }
}

/// The `[machine]` table of a `host.toml`'s text, `label` naming the file
/// in the warnings; `None` when it has none. A value of the wrong shape
/// keeps its default and is a warning.
pub fn parse_host_machine(text: &str, label: &str) -> Option<(MachineResources, Vec<String>)> {
    let mut found: Option<(MachineResources, Vec<String>)> = None;
    let mut in_machine = false;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(header) = line.strip_prefix('[') {
            let name = strip_comment(header).strip_suffix(']').map(str::trim);
            in_machine = name == Some("machine") && found.is_none();
            if in_machine {
                found = Some((MachineResources::default(), Vec::new()));
            }
            continue;
        }
        let Some((resources, warnings)) = found.as_mut().filter(|_| in_machine) else {
            continue;
        };
        let at = format!("{label}:{}", index + 1);
        let Some((key, value)) = line.split_once('=') else {
            warnings.push(format!("{at}: expected KEY = value"));
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        let parsed = parse_positive(value, "number");
        match (key, parsed) {
            ("cpus", Ok(n)) => match u32::try_from(n) {
                Ok(n) => resources.cpus = n,
                Err(_) => warnings.push(format!("{at}: cpus: {n} is too large")),
            },
            ("memory_mib", Ok(n)) => resources.memory_mib = n.unsigned_abs(),
            ("disk_gib", Ok(n)) => resources.disk_gib = n.unsigned_abs(),
            ("cpus" | "memory_mib" | "disk_gib", Err(error)) => {
                warnings.push(format!("{at}: {key}: {error:#}; its default holds"));
            }
            _ => warnings.push(format!(
                "{at}: unknown key {key} in [machine]; the keys are cpus, memory_mib, disk_gib"
            )),
        }
    }
    found
}

/// The machine's resources: the `[machine]` of the host-wide `host.toml`
/// (`host_wide`), else the defaults, with what was wrong in it. Only the
/// host-wide file counts: the machine is the host's, not a queue's.
pub fn load_machine_resources(host_wide: Option<&Path>) -> (MachineResources, Vec<String>) {
    let Some(path) = host_wide else {
        return (MachineResources::default(), Vec::new());
    };
    match fs::read_to_string(path) {
        Ok(text) => parse_host_machine(&text, &path.display().to_string())
            .unwrap_or_else(|| (MachineResources::default(), Vec::new())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            (MachineResources::default(), Vec::new())
        }
        Err(error) => (
            MachineResources::default(),
            vec![format!("read {}: {error}", path.display())],
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::super::kpi_config::HOST_FILE_NAME;
    use super::*;

    #[test]
    fn host_toml_overrides_the_machine_s_resources() {
        let (resources, warnings) = parse_host_machine(
            "[update]\ncpus = 9\n[machine]\ncpus = 6 # measured\nmemory_mib = 12_288\n",
            "host.toml",
        )
        .unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            resources,
            MachineResources {
                cpus: 6,
                memory_mib: 12288,
                ..MachineResources::default()
            }
        );
        // Without the table, the defaults.
        assert_eq!(parse_host_machine("[update]\n", "host.toml"), None);
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            load_machine_resources(Some(&dir.path().join("none.toml"))),
            (MachineResources::default(), Vec::new())
        );
        let file = dir.path().join(HOST_FILE_NAME);
        fs::write(&file, "[machine]\ndisk_gib = 200\n").unwrap();
        assert_eq!(load_machine_resources(Some(&file)).0.disk_gib, 200);
    }

    #[test]
    fn a_wrong_value_keeps_its_default_and_is_a_warning() {
        let (resources, warnings) = parse_host_machine(
            "[machine]\ncpus = 0\nmemory_mib = \"lots\"\nswap = 1\nnot a pair\n",
            "h",
        )
        .unwrap();
        assert_eq!(resources, MachineResources::default());
        assert_eq!(warnings.len(), 4, "{warnings:?}");
        assert!(warnings[0].starts_with("h:2: cpus:"), "{warnings:?}");
        assert!(warnings[2].contains("unknown key swap"), "{warnings:?}");
    }

    #[test]
    fn init_mounts_each_volume_at_its_path_and_names_the_machine() {
        let args = init_args(
            &MachineResources::default(),
            &[PathBuf::from("/q/a/runs"), PathBuf::from("/q/a/clones")],
        );
        let args: Vec<String> = args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "machine",
                "init",
                "--cpus",
                "2",
                "--memory",
                "6144",
                "--disk-size",
                "60",
                "--volume",
                "/q/a/runs:/q/a/runs",
                "--volume",
                "/q/a/clones:/q/a/clones",
                "dagq",
            ]
        );
    }

    #[test]
    fn podman_s_json_reads_as_machines_connections_and_mounts() {
        let machines = parse_machine_list(
            r#"[{"Name":"podman-machine-default*","Default":true,"Running":true},
                {"Name":"dagq","Running":false,"Starting":false}]"#,
        )
        .unwrap();
        assert_eq!(machines[0].name, "podman-machine-default");
        assert!(machines[0].running && !machines[1].running);
        assert!(parse_machine_list("null").unwrap().is_empty());
        assert_eq!(
            parse_default_connection(
                r#"[{"Name":"a","Default":false},{"Name":"b","Default":true}]"#
            )
            .unwrap()
            .as_deref(),
            Some("b")
        );
        assert_eq!(parse_default_connection("[]").unwrap(), None);
        assert_eq!(
            config_file(r#"[{"ConfigDir":{"Path":"/c/applehv"},"Name":"dagq"}]"#).unwrap(),
            Path::new("/c/applehv/dagq.json")
        );
        assert!(config_file("[]").is_err());
        assert_eq!(
            parse_config_mounts(r#"{"Mounts":[{"Source":"/q/a/runs","Target":"/q/a/runs"}]}"#)
                .unwrap(),
            [PathBuf::from("/q/a/runs")]
        );
        assert!(
            parse_config_mounts(r#"{"Mounts":null}"#)
                .unwrap()
                .is_empty()
        );
        assert!(parse_config_mounts(r#"{"Mounts":[{}]}"#).is_err());
    }

    #[test]
    fn a_new_machine_mounts_every_queue_of_the_host() {
        let root = tempfile::tempdir().unwrap();
        for queue in ["a", "b"] {
            fs::create_dir_all(root.path().join(queue)).unwrap();
            fs::write(root.path().join(queue).join(DB_FILE_NAME), "").unwrap();
        }
        // Not a queue: no database.
        fs::create_dir_all(root.path().join(MACHINE_DIR_NAME)).unwrap();
        let volumes = host_volumes(root.path(), &root.path().join("a"));
        let names: Vec<String> = volumes
            .iter()
            .map(|dir| dir.strip_prefix(root.path()).unwrap().display().to_string())
            .collect();
        assert_eq!(
            names,
            [
                "a/clones",
                "a/runs",
                "a/verification",
                "b/clones",
                "b/runs",
                "b/verification"
            ]
        );
    }

    #[test]
    fn users_whose_owner_is_over_are_not_counted() {
        let dir = tempfile::tempdir().unwrap();
        let users = HostMachineUsers {
            dir: dir.path().to_path_buf(),
            runs: None,
        };
        let me = LeaseOwner::Process {
            pid: std::process::id(),
        };
        assert_eq!(users.count().unwrap(), 0);
        users.join("r1", &me).unwrap();
        users.join("r2", &me).unwrap();
        assert_eq!(users.count().unwrap(), 2);
        // A process that ended, a run of a queue that is gone, a record
        // nobody reads, and a bare pid of a binary from before the owners.
        users
            .join(
                "gone",
                &LeaseOwner::Process {
                    pid: i32::MAX as u32,
                },
            )
            .unwrap();
        let no_queue = LeaseOwner::Run {
            db: dir.path().join("none/queue.db"),
            run: "r9".into(),
        };
        users.join("r9", &no_queue).unwrap();
        fs::write(dir.path().join("users/junk"), "{").unwrap();
        fs::write(dir.path().join("users/old"), std::process::id().to_string()).unwrap();
        assert_eq!(users.count().unwrap(), 3);
        for left in ["gone", "r9", "junk"] {
            assert!(!dir.path().join("users").join(left).exists(), "{left}");
        }
        users.leave("r1").unwrap();
        users.leave("r1").unwrap();
        users.leave("old").unwrap();
        assert_eq!(users.count().unwrap(), 1);
        assert!(users.join("../x", &me).is_err());
    }

    /// The machine's memory is its own (what it was created with), and its
    /// containers' limits are read in MiB, a relay told by its label.
    #[test]
    fn podman_s_json_reads_as_the_machine_s_memory_and_its_containers() {
        assert_eq!(
            parse_machine_memory(r#"[{"Name":"dagq","Resources":{"CPUs":6,"Memory":10240}}]"#)
                .unwrap(),
            10240
        );
        assert!(parse_machine_memory(r#"[{"Name":"dagq"}]"#).is_err());
        let containers = parse_container_inspect(
            r#"[
              {"Name":"dagq-q-run-r1","HostConfig":{"Memory":8589934592},"Config":{"Labels":{"dagq.queue":"q","dagq.run":"r1"}}},
              {"Name":"/dagq-q-relay-r1","HostConfig":{"Memory":0},"Config":{"Labels":{"dagq.queue":"q","dagq.relay.run":"r1"}}},
              {"Name":"dagq-q-verify-v","HostConfig":{"Memory":1},"Config":{"Labels":null}}
            ]"#,
        )
        .unwrap();
        assert_eq!(
            containers,
            [
                MachineContainer {
                    name: "dagq-q-run-r1".into(),
                    memory_mib: Some(8192),
                    relay: false
                },
                MachineContainer {
                    name: "dagq-q-relay-r1".into(),
                    memory_mib: None,
                    relay: true
                },
                MachineContainer {
                    name: "dagq-q-verify-v".into(),
                    memory_mib: Some(1),
                    relay: false
                },
            ]
        );
        assert!(parse_container_inspect("{}").is_err());
    }

    /// The reservations and the waits are host-wide files beside the
    /// users: a wait keeps when it began, a reservation takes its place,
    /// and a record nobody reads is forgotten.
    #[test]
    fn the_reservations_and_the_waits_are_kept_beside_the_users() {
        use crate::domain::machine_memory::Purpose;
        let dir = tempfile::tempdir().unwrap();
        let users = HostMachineUsers {
            dir: dir.path().to_path_buf(),
            runs: None,
        };
        let me = LeaseOwner::Process {
            pid: std::process::id(),
        };
        let ask = |container: &str, since| MemoryAsk {
            container: container.into(),
            purpose: Purpose::Turn,
            memory_mib: 4096,
            owner: me.clone(),
            pid: std::process::id(),
            since,
            seq: 0,
            started: None,
        };
        assert!(users.reservations().unwrap().is_empty());
        users.wait(&ask("dagq-q-run-r1", 5)).unwrap();
        users.wait(&ask("dagq-q-run-r1", 9)).unwrap();
        users.wait(&ask("dagq-q-run-r2", 7)).unwrap();
        let waits = users.waits().unwrap();
        assert_eq!(waits.len(), 2);
        assert_eq!(
            (waits[0].container.as_str(), waits[0].since),
            ("dagq-q-run-r1", 5)
        );
        users.reserve(&ask("dagq-q-run-r1", 20)).unwrap();
        assert_eq!(users.waits().unwrap().len(), 1);
        assert_eq!(users.reservations().unwrap()[0].since, 20);
        users.unreserve("dagq-q-run-r1").unwrap();
        users.unreserve("dagq-q-run-r1").unwrap();
        users.unwait("dagq-q-run-r2").unwrap();
        assert!(users.reservations().unwrap().is_empty());
        assert!(users.waits().unwrap().is_empty());
        // Neither a record being written nor one nobody reads is an ask,
        // and a read removes neither.
        fs::write(dir.path().join("waiting/junk"), "{").unwrap();
        fs::write(dir.path().join("waiting/dagq-q-run-r5.tmp"), "{").unwrap();
        assert!(users.waits().unwrap().is_empty());
        assert!(dir.path().join("waiting/junk").exists());
        assert!(users.wait(&ask("../x", 1)).is_err());
        // The lock is not waited for by a try while another holds it.
        let held = users.lock().unwrap();
        assert!(users.try_lock().unwrap().is_none());
        drop(held);
        assert!(users.try_lock().unwrap().is_some());
        assert!(users.owner_held(&me));
        assert!(!users.owner_held(&LeaseOwner::Process {
            pid: i32::MAX as u32
        }));
    }

    /// Runs that own users and are read through the queue service: the
    /// queue it serves answers, and another queue is unknown.
    struct ServedRuns {
        db: PathBuf,
        run: RelayRun,
    }

    impl OwnerRuns for ServedRuns {
        fn run(&self, db: &Path, _run: &str) -> Option<RelayRun> {
            (db == self.db).then_some(self.run)
        }
    }

    /// A process that opens no queue reads the runs that own users through
    /// the service it is a client of, never on a queue's DB (here a file
    /// no SQLite opens, which would read as unknown): the served queue's
    /// run that is over leaves, another queue's, which it does not reach,
    /// stays counted, and one of a queue whose database is gone leaves.
    #[test]
    fn the_users_runs_are_read_through_the_service_and_no_queue_is_opened() {
        let dir = tempfile::tempdir().unwrap();
        let served = dir.path().join("served").join("queue.db");
        let other = dir.path().join("other").join("queue.db");
        let gone = dir.path().join("gone").join("queue.db");
        for db in [&served, &other] {
            fs::create_dir_all(db.parent().unwrap()).unwrap();
            fs::write(db, "not a queue").unwrap();
        }
        let users = HostMachineUsers {
            dir: dir.path().join("machine"),
            runs: Some(std::sync::Arc::new(ServedRuns {
                db: served.clone(),
                run: RelayRun::Missing,
            })),
        };
        for (user, db) in [("r1", &served), ("r2", &other), ("r3", &gone)] {
            users
                .join(
                    user,
                    &LeaseOwner::Run {
                        db: db.clone(),
                        run: user.into(),
                    },
                )
                .unwrap();
        }
        assert_eq!(users.count().unwrap(), 1);
        let left: Vec<_> = fs::read_dir(users.users())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(left, ["r2"]);
        assert!(users.owner_held(&LeaseOwner::Run {
            db: other.clone(),
            run: "r2".into(),
        }));
        assert!(!users.owner_held(&LeaseOwner::Run {
            db: served,
            run: "r1".into(),
        }));
        assert!(!users.owner_held(&LeaseOwner::Run {
            db: gone,
            run: "r3".into(),
        }));
    }

    #[test]
    fn a_run_whose_queue_cannot_be_looked_at_stays_counted() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let users = HostMachineUsers {
            dir: dir.path().join("machine"),
            runs: None,
        };
        let shut = dir.path().join("shut");
        fs::create_dir(&shut).unwrap();
        fs::write(shut.join("queue.db"), "").unwrap();
        let owner = LeaseOwner::Run {
            db: shut.join("queue.db"),
            run: "r1".into(),
        };
        users.join("r1", &owner).unwrap();
        // The queue's directory cannot be entered: whether the queue is
        // there is not known, so the run is not taken to be over.
        fs::set_permissions(&shut, fs::Permissions::from_mode(0o000)).unwrap();
        let counted = users.count();
        fs::set_permissions(&shut, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(counted.unwrap(), 1);
        assert!(dir.path().join("machine/users/r1").exists());
        // Gone once it is looked at and is not there.
        fs::remove_dir_all(&shut).unwrap();
        assert_eq!(users.count().unwrap(), 0);
    }
}

/// A queue's runs' parts in the dagq machine ([`RunMachine`]): the
/// machine of [`QueueMachine`], the image of the main checkout
/// (`CheckoutImages`), the run's relay towards the queue's broker
/// (`PodmanRelays`), the run's build output volume and the queue's worker
/// cache volumes ([`QueueVolumes`]).
#[derive(Clone)]
pub struct QueueRunMachine {
    pub podman: PodmanCli,
    pub db: PathBuf,
    /// The main checkout the queue is bound to, whose `dagq.toml` and
    /// files decide the image; `None` readies nothing.
    pub main_checkout: Option<PathBuf>,
    /// The host-wide `host.toml`: the machine's resources.
    pub host_wide: Option<PathBuf>,
    /// Where the image's records go when the process opens no queue: kept
    /// for it to send (a command of the host's `dagq` the queue service
    /// runs), or sent to the queue service as they are made (a run's
    /// session wrapper, a person's `integrate` through the service); on the
    /// queue at `db` without.
    pub kept_records:
        Option<std::sync::Arc<dyn crate::application::container_image::ImageRecords + Send + Sync>>,
    /// How the runs that own users of the machine are read when the
    /// process opens no queue ([`HostMachineUsers::runs`]).
    pub owner_runs: Option<std::sync::Arc<dyn OwnerRuns>>,
}

impl std::fmt::Debug for QueueRunMachine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueueRunMachine")
            .field("podman", &self.podman)
            .field("db", &self.db)
            .field("main_checkout", &self.main_checkout)
            .field("host_wide", &self.host_wide)
            .field("kept_records", &self.kept_records.is_some())
            .field("owner_runs", &self.owner_runs.is_some())
            .finish()
    }
}

impl crate::application::podman_machine::RunMachine for QueueRunMachine {
    fn ready(&self, run: &str, build_output: Option<&str>) -> Result<String> {
        let image = self.ready_image(run)?;
        let relays = super::broker_relay::PodmanRelays::of_queue(
            self.podman.clone(),
            &self.db,
            self.host_wide.as_deref(),
        )
        .with_owner_runs(self.owner_runs.clone());
        if !relays.is_open(run)? {
            crate::application::broker_relay::open(
                &relays,
                &super::queue_broker::SystemQueueBroker::new(&self.db, Path::new("dagq")),
                &crate::domain::broker_relay::RelaySpec {
                    run: run.to_owned(),
                    image: image.clone(),
                    uid: crate::domain::broker_relay::CONTAINER_UID,
                },
            )?;
        }
        if let Some(volume) = build_output {
            let queue_dir = super::queue_service::queue_dir_of(&self.db);
            QueueVolumes::of_queue(self.podman.clone(), &queue_dir)?.create_build_output(volume)?;
        }
        Ok(image)
    }

    fn ready_image(&self, run: &str) -> Result<String> {
        let checkout = self
            .main_checkout
            .as_deref()
            .context("the queue is bound to no checkout, whose dagq.toml decides the image")?;
        let queue_dir = super::queue_service::queue_dir_of(&self.db);
        let data_root = queue_dir.parent().unwrap_or(Path::new("."));
        let mut machine = QueueMachine::new(
            self.podman.clone(),
            data_root,
            &queue_dir,
            self.host_wide.as_deref(),
        );
        machine.users.runs.clone_from(&self.owner_runs);
        // Absolute: another process, whatever its cwd, reads the queue.
        let owner = LeaseOwner::Run {
            db: fs::canonicalize(&self.db).unwrap_or_else(|_| self.db.clone()),
            run: run.to_owned(),
        };
        crate::application::podman_machine::ensure(&machine.machine(), run, &owner)
            .map_err(anyhow::Error::msg)
            .context("make the dagq machine run")?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| {
                i64::try_from(since.as_secs()).unwrap_or(i64::MAX)
            });
        let queue_records = super::container_image::QueueImageRecords {
            db: self.db.clone(),
            actor: None,
        };
        let records: &dyn crate::application::container_image::ImageRecords =
            match &self.kept_records {
                Some(kept) => &**kept,
                None => &queue_records,
            };
        let image = crate::application::container_image::prepare(
            &super::container_image::CheckoutImages::new(checkout, &queue_dir),
            &super::container_image::PodmanImages {
                podman: self.podman.clone(),
            },
            records,
            now,
        )
        .map_err(anyhow::Error::msg)?;
        Ok(image)
    }

    fn remove_build_output(&self, build_output: &str) -> Result<()> {
        let queue_dir = super::queue_service::queue_dir_of(&self.db);
        QueueVolumes::of_queue(self.podman.clone(), &queue_dir)?.remove_build_output(build_output)
    }

    fn create_worker_cache(&self, dirs: &[String]) -> Result<()> {
        if dirs.is_empty() {
            return Ok(());
        }
        let queue_dir = super::queue_service::queue_dir_of(&self.db);
        let volumes = QueueVolumes::of_queue(self.podman.clone(), &queue_dir)?;
        dirs.iter()
            .try_for_each(|dir| volumes.create_worker_cache(dir))
    }

    fn reserve(&self, ask: &MemoryAsk, now: i64) -> Result<MemoryStep> {
        let (users, memory) = self.memory_parts();
        crate::application::podman_machine::try_reserve(
            &MemoryMachine {
                users: &users,
                memory: &memory,
                records: &users,
            },
            ask,
            now,
        )
    }

    fn release_memory(&self, container: &str) -> Result<()> {
        let (users, memory) = self.memory_parts();
        crate::application::podman_machine::release_memory(
            &MemoryMachine {
                users: &users,
                memory: &memory,
                records: &users,
            },
            container,
        )
    }

    fn mark_started(&self, container: &str, now: i64) -> Result<()> {
        let (users, memory) = self.memory_parts();
        crate::application::podman_machine::mark_started(
            &MemoryMachine {
                users: &users,
                memory: &memory,
                records: &users,
            },
            container,
            now,
        )
    }

    fn release_memory_if_free(&self, container: &str) -> Result<()> {
        let (users, memory) = self.memory_parts();
        crate::application::podman_machine::release_memory_if_free(
            &MemoryMachine {
                users: &users,
                memory: &memory,
                records: &users,
            },
            container,
        )
        .map(drop)
    }
}

impl QueueRunMachine {
    /// The machine's host-wide records and its reads of what it holds.
    fn memory_parts(&self) -> (HostMachineUsers, PodmanCliWithin) {
        let queue_dir = super::queue_service::queue_dir_of(&self.db);
        let data_root = queue_dir.parent().unwrap_or(Path::new("."));
        (
            HostMachineUsers {
                dir: data_root.join(MACHINE_DIR_NAME),
                runs: self.owner_runs.clone(),
            },
            PodmanCliWithin {
                podman: self.podman.clone(),
                limit: MEMORY_LOOK_TIMEOUT,
            },
        )
    }
}

/// Now in Unix milliseconds: the clock of a wait for the machine's memory.
pub(super) fn unix_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_millis()).unwrap_or(i64::MAX)
        })
}

/// Now in Unix seconds.
pub(super) fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_secs()).unwrap_or(i64::MAX)
        })
}

/// How the dagq machine's memory looks for a new claim of a queue
/// ([`MemoryRoom`]): the machine and its containers read through
/// `podman`, the host-wide records in `users`, and the limits and the
/// machine's resources of the host-wide `host.toml` (`host_wide`).
#[derive(Debug, Clone)]
pub struct HostMemoryRoom {
    pub podman: PodmanCli,
    pub users: HostMachineUsers,
    pub host_wide: Option<PathBuf>,
}

impl HostMemoryRoom {
    /// The room of the queues under `data_root` (`<data home>/dagq`).
    pub fn under(data_root: &Path, host_wide: Option<PathBuf>) -> Self {
        Self {
            podman: PodmanCli::default(),
            users: HostMachineUsers {
                dir: data_root.join(MACHINE_DIR_NAME),
                runs: None,
            },
            host_wide,
        }
    }

    /// This room with the runs that own users of the machine read through
    /// `runs` when given ([`HostMachineUsers::runs`]).
    pub fn with_owner_runs(mut self, runs: Option<std::sync::Arc<dyn OwnerRuns>>) -> Self {
        self.users.runs = runs;
        self
    }
}

impl MemoryRoom for HostMemoryRoom {
    fn look(&self) -> RoomLook {
        let within = PodmanCliWithin {
            podman: self.podman.clone(),
            limit: MEMORY_LOOK_TIMEOUT,
        };
        let limit = super::run_container::load_container_limits(self.host_wide.as_deref())
            .0
            .memory_mib;
        crate::application::podman_machine::look_room(
            &within,
            &within,
            &self.users,
            &load_machine_resources(self.host_wide.as_deref()).0,
            limit,
            unix_now(),
        )
    }
}
