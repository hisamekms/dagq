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
//! (held with `flock` while the machine is started or stopped) and
//! `users/<user>` (the pid of the process that joined).

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
use crate::application::podman_machine::{Machine, MachineUsers, MachineVolumes, PodmanMachine};
use crate::domain::podman_machine::{
    MACHINE_NAME, MachineEntry, MachineMounts, MachineResources, QueueKey,
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
#[derive(Debug, Clone)]
pub struct HostMachineUsers {
    pub dir: PathBuf,
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

    fn join(&self, user: &str) -> Result<()> {
        let file = self.user_file(user)?;
        fs::create_dir_all(self.users())?;
        fs::write(&file, std::process::id().to_string())
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
        for entry in entries.flatten() {
            let alive = fs::read_to_string(entry.path())
                .ok()
                .and_then(|pid| pid.trim().parse().ok())
                .is_some_and(super::adapters::process_alive);
            if alive {
                count += 1;
            } else {
                // Its process ended without leaving.
                let _ = fs::remove_file(entry.path());
            }
        }
        Ok(count)
    }
}

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

    fn create_worker_cache(&self) -> Result<()> {
        self.create(&self.queue.worker_cache_volume())
    }

    fn remove_worker_cache(&self) -> Result<()> {
        self.remove(&self.queue.worker_cache_volume())
    }

    fn worker_cache_exists(&self) -> Result<bool> {
        self.exists(&self.queue.worker_cache_volume())
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
                "4096",
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
    fn users_whose_process_ended_are_not_counted() {
        let dir = tempfile::tempdir().unwrap();
        let users = HostMachineUsers {
            dir: dir.path().to_path_buf(),
        };
        assert_eq!(users.count().unwrap(), 0);
        users.join("r1").unwrap();
        users.join("r2").unwrap();
        assert_eq!(users.count().unwrap(), 2);
        // A pid no process has.
        fs::write(dir.path().join("users/gone"), i32::MAX.to_string()).unwrap();
        assert_eq!(users.count().unwrap(), 2);
        assert!(!dir.path().join("users/gone").exists());
        users.leave("r1").unwrap();
        users.leave("r1").unwrap();
        assert_eq!(users.count().unwrap(), 1);
        assert!(users.join("../x").is_err());
    }
}

/// A queue's runs' parts in the dagq machine ([`RunMachine`]): the
/// machine of [`QueueMachine`], the image of the main checkout
/// (`CheckoutImages`), the run's relay towards the queue's broker
/// (`PodmanRelays`) and the run's build output volume ([`QueueVolumes`]).
#[derive(Debug, Clone)]
pub struct QueueRunMachine {
    pub podman: PodmanCli,
    pub db: PathBuf,
    /// The main checkout the queue is bound to, whose `dagq.toml` and
    /// files decide the image; `None` readies nothing.
    pub main_checkout: Option<PathBuf>,
    /// The host-wide `host.toml`: the machine's resources.
    pub host_wide: Option<PathBuf>,
}

impl crate::application::podman_machine::RunMachine for QueueRunMachine {
    fn ready(&self, run: &str, build_output: bool) -> Result<String> {
        let checkout = self
            .main_checkout
            .as_deref()
            .context("the queue is bound to no checkout, whose dagq.toml decides the image")?;
        let queue_dir = super::queue_service::queue_dir_of(&self.db);
        let data_root = queue_dir.parent().unwrap_or(Path::new("."));
        let machine = QueueMachine::new(
            self.podman.clone(),
            data_root,
            &queue_dir,
            self.host_wide.as_deref(),
        );
        crate::application::podman_machine::ensure(&machine.machine(), run)
            .map_err(anyhow::Error::msg)
            .context("make the dagq machine run")?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| {
                i64::try_from(since.as_secs()).unwrap_or(i64::MAX)
            });
        let image = crate::application::container_image::prepare(
            &super::container_image::CheckoutImages::new(checkout, &queue_dir),
            &super::container_image::PodmanImages {
                podman: self.podman.clone(),
            },
            &super::container_image::QueueImageRecords {
                db: self.db.clone(),
                actor: None,
            },
            now,
        )
        .map_err(anyhow::Error::msg)?;
        let relays = super::broker_relay::PodmanRelays::of_queue(
            self.podman.clone(),
            &self.db,
            self.host_wide.as_deref(),
        );
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
        if build_output {
            QueueVolumes::of_queue(self.podman.clone(), &queue_dir)?.create_build_output(run)?;
        }
        Ok(image)
    }
}
