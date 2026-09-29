//! The resource broker's container and its Podman machine ([Broker]
//! container and Podman machine, ADR-t827-3): [`start`] makes sure, in
//! order and each step idempotent, that dagq's own machine exists and runs
//! ([`ensure_machine`]), that the image of this build is there, that the
//! queue's container runs, and that it answers health on `127.0.0.1`;
//! [`stop`] stops the container and then the machine when no container
//! runs on it ([`release_machine`]); [`status`] reads all of it without
//! changing anything.
//!
//! Podman is behind the [`Podman`] port, so the decisions here (which
//! podman command comes next from the state podman reports) and the
//! arguments they build ([`MachineSpec::init_args`],
//! [`ContainerSpec::run_args`]) are tested without podman. The machine's
//! steps run under a host-wide [`HostLock`], so tests and runs calling them
//! at once do not init or stop it twice. dagq touches only the machine
//! named [`MACHINE`] and talks to it by naming its connection on every
//! command: a person's default machine and default connection are never
//! started, stopped or changed.
//!
//! A failure is a [`BrokerFailure`] with a [`FailureCode`], never a silent
//! fallback: no podman, a person's machine running, a build or a container
//! that fails, a health that does not answer.
//!
//! [Broker]: ../../docs/design/broker.md

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The name of dagq's own Podman machine, and of its connection.
pub const MACHINE: &str = "dagq";
/// The image's repository; the tag is the build tag ([`image_tag`]).
pub const IMAGE_REPOSITORY: &str = "localhost/dagq-broker";
/// The container of a queue is this plus the queue hash.
pub const CONTAINER_PREFIX: &str = "dagq-broker-";
/// The port the server listens on inside its container.
pub const CONTAINER_PORT: u16 = 8750;
/// The address the server binds inside its container: the container's own
/// interfaces, which are private to its network namespace. The host
/// publishes the port on `127.0.0.1` only.
pub const CONTAINER_LISTEN: &str = "0.0.0.0";
/// How long [`start`] waits for the health to answer.
pub const HEALTH_TIMEOUT: Duration = Duration::from_secs(30);
/// The build's parallelism in the image's build stage.
pub const BUILD_JOBS: u32 = 1;
/// The build argument that gives the image's build stage dagq's build
/// identifier, which the server embeds in place of its own (the build stage
/// has no Git), so its health names dagq's build.
pub const IMAGE_BUILD_ARG: &str = dagq_broker_protocol::build_id::GIVEN_ENV;
/// The file name of the worker's client, next to `dagq` (ADR-t827-1
/// decision 5).
pub const CLIENT_BINARY: &str = "dagq-broker-client";

/// The client that goes with the `dagq` at `dagq`: `dagq-broker-client` in
/// the same directory.
pub fn client_path(dagq: &Path) -> PathBuf {
    dagq.with_file_name(CLIENT_BINARY)
}

/// The client dagq hands a worker (ADR-t827-1 decisions 5 and 7): the one
/// next to the `dagq` at `dagq`, and only when its build identifier
/// (`version`, its `--version`) is `build`, dagq's own. A missing client or
/// one of another build is a [`BrokerFailure`], never a fallback to it.
pub fn resolve_client(
    dagq: &Path,
    build: &str,
    version: &dyn Fn(&Path) -> Result<String, String>,
) -> BrokerResult<PathBuf> {
    let client = client_path(dagq);
    if !client.is_file() {
        return Err(BrokerFailure::new(
            FailureCode::ClientMissing,
            format!(
                "no {CLIENT_BINARY} at {}: install puts it next to dagq",
                client.display()
            ),
        ));
    }
    match version(&client) {
        Ok(found) if found == build => Ok(client),
        Ok(found) => Err(BrokerFailure::new(
            FailureCode::VersionMismatch,
            format!(
                "{} is build {found}, not dagq's {build}: run `dagq install` to put both in place",
                client.display()
            ),
        )),
        Err(error) => Err(BrokerFailure::new(
            FailureCode::VersionMismatch,
            format!("{} does not report its build: {error}", client.display()),
        )),
    }
}

/// The client as `status` and `doctor` report it: where it is looked for,
/// the build it names, and whether dagq would use it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClientReport {
    pub path: PathBuf,
    pub build: Option<String>,
    pub matches: bool,
    pub error: Option<Value>,
}

/// [`resolve_client`] as a [`ClientReport`].
pub fn client_report(
    dagq: &Path,
    build: &str,
    version: &dyn Fn(&Path) -> Result<String, String>,
) -> ClientReport {
    let path = client_path(dagq);
    let found = path.is_file().then(|| version(&path).ok()).flatten();
    match resolve_client(dagq, build, version) {
        Ok(path) => ClientReport {
            path,
            build: found,
            matches: true,
            error: None,
        },
        Err(failure) => ClientReport {
            path,
            build: found,
            matches: false,
            error: Some(failure.to_json()),
        },
    }
}

/// Why a broker step failed, as `dagq broker` and a later status report it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureCode {
    /// The podman executable was not found (a person installs it:
    /// `brew install podman`).
    PodmanMissing,
    /// A podman command failed in a way no other code names.
    PodmanFailed,
    /// Another machine runs, so dagq's cannot start (the person's machine
    /// is not stopped for it).
    MachineBusy,
    /// `podman machine init` or `start` failed.
    MachineFailed,
    /// dagq's machine does not exist, for a command that only reads
    /// (`dagq broker logs`) and so does not make it.
    MachineMissing,
    /// dagq's machine is stopped, for a command that only reads.
    MachineStopped,
    /// The image's material (a dagq checkout with the broker's crates) is
    /// not there.
    ImageSourceMissing,
    ImageBuildFailed,
    ContainerFailed,
    /// The queue's container does not exist, for a command that only
    /// reads (`dagq broker logs`) and so does not make it.
    ContainerMissing,
    /// The container runs but its health did not answer in time.
    Unhealthy,
    /// The queue belongs to no repository dagq knows, so the Git common
    /// dir to mount is unknown.
    RepositoryUnknown,
    /// No `dagq-broker-client` next to dagq (ADR-t827-1 decision 5).
    ClientMissing,
    /// The client, or the broker's health, names another build than
    /// dagq's: dagq does not use it (fail closed, ADR-t827-1 decision 7).
    VersionMismatch,
}

impl FailureCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PodmanMissing => "podman_missing",
            Self::PodmanFailed => "podman_failed",
            Self::MachineBusy => "machine_busy",
            Self::MachineFailed => "machine_failed",
            Self::MachineMissing => "machine_missing",
            Self::MachineStopped => "machine_stopped",
            Self::ImageSourceMissing => "image_source_missing",
            Self::ImageBuildFailed => "image_build_failed",
            Self::ContainerFailed => "container_failed",
            Self::ContainerMissing => "container_missing",
            Self::Unhealthy => "unhealthy",
            Self::RepositoryUnknown => "repository_unknown",
            Self::ClientMissing => "client_missing",
            Self::VersionMismatch => "version_mismatch",
        }
    }
}

/// A broker step's failure: its code and a message for a person, which
/// names what failed and podman's own words, never a token or a key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BrokerFailure {
    pub code: FailureCode,
    pub message: String,
}

impl BrokerFailure {
    pub fn new(code: FailureCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// The error's JSON, `{"code","message"}`.
    pub fn to_json(&self) -> Value {
        serde_json::json!({"code": self.code.as_str(), "message": self.message})
    }
}

impl fmt::Display for BrokerFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "broker {}: {}", self.code.as_str(), self.message)
    }
}

impl std::error::Error for BrokerFailure {}

pub type BrokerResult<T> = Result<T, BrokerFailure>;

/// What a podman command printed and how it ended.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PodmanOutput {
    pub success: bool,
    /// The exit code, `None` when a signal ended podman.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl PodmanOutput {
    /// The output's words for a message: stderr, else stdout, trimmed.
    fn words(&self) -> &str {
        let stderr = self.stderr.trim();
        if stderr.is_empty() {
            self.stdout.trim()
        } else {
            stderr
        }
    }
}

/// The podman executable. `run` fails only when podman cannot be run at
/// all (`podman_missing`); a command that ran and failed is an output that
/// is not `success`.
pub trait Podman {
    fn run(&self, args: &[String]) -> BrokerResult<PodmanOutput>;
}

/// The host-wide lock of dagq's machine: held while the machine is
/// inited, started or stopped, released when the guard drops.
pub trait HostLock {
    fn hold(&self) -> BrokerResult<Box<dyn std::any::Any>>;
}

/// Asks the broker's health on `127.0.0.1:<port>`.
pub trait HealthProbe {
    fn probe(&self, port: u16) -> Result<dagq_broker_protocol::HealthResponse, String>;
}

fn args<const N: usize>(words: [&str; N]) -> Vec<String> {
    words.iter().map(|word| (*word).to_owned()).collect()
}

/// The arguments of a podman command on dagq's machine: the connection is
/// named on every one, so the person's default connection is never used.
fn on_machine(machine: &str, rest: &[&str]) -> Vec<String> {
    let mut all = args(["--connection", machine]);
    all.extend(rest.iter().map(|word| (*word).to_owned()));
    all
}

/// Whether what `args` (`image exists`, `container exists`) asks about
/// exists: exit 0 is yes, exit 1 is no, anything else (the machine not
/// answering, say) is `podman_failed` rather than taken for "absent".
fn exists(podman: &dyn Podman, args: &[String], what: &str) -> BrokerResult<bool> {
    let output = podman.run(args)?;
    match (output.success, output.code) {
        (true, _) => Ok(true),
        (false, Some(1)) => Ok(false),
        (false, _) => Err(BrokerFailure::new(
            FailureCode::PodmanFailed,
            format!("{what} failed: {}", output.words()),
        )),
    }
}

/// Run `args`, failing with `code` and `what` when podman ran and failed.
fn checked(
    podman: &dyn Podman,
    args: &[String],
    code: FailureCode,
    what: &str,
) -> BrokerResult<PodmanOutput> {
    let output = podman.run(args)?;
    if output.success {
        Ok(output)
    } else {
        Err(BrokerFailure::new(
            code,
            format!("{what} failed: {}", output.words()),
        ))
    }
}

// ---------------------------------------------------------------------------
// The machine.

/// dagq's machine and its resources (ADR-t827-3 decision 7): the fewest
/// with which the image builds and the broker runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MachineSpec {
    pub name: String,
    pub cpus: u32,
    pub memory_mib: u32,
    pub disk_gib: u32,
}

impl Default for MachineSpec {
    fn default() -> Self {
        Self {
            name: MACHINE.to_owned(),
            cpus: 1,
            memory_mib: 1024,
            disk_gib: 10,
        }
    }
}

impl MachineSpec {
    /// The defaults with the host's overrides of `host.toml`'s `[broker]`
    /// (ADR-t827-3 decision 7): the name stays dagq's.
    pub fn with_host(host: &crate::domain::broker::HostBroker) -> Self {
        let default = Self::default();
        Self {
            cpus: host.machine_cpus.unwrap_or(default.cpus),
            memory_mib: host.machine_memory_mib.unwrap_or(default.memory_mib),
            disk_gib: host.machine_disk_gib.unwrap_or(default.disk_gib),
            ..default
        }
    }

    /// `podman machine init`: rootless, the default volumes (ADR-t827-3
    /// decision 6), and `--update-connection=false` so the machine does
    /// not become the default connection even when it is the first.
    pub fn init_args(&self) -> Vec<String> {
        let mut all = args(["machine", "init"]);
        all.extend([
            "--cpus".to_owned(),
            self.cpus.to_string(),
            "--memory".to_owned(),
            self.memory_mib.to_string(),
            "--disk-size".to_owned(),
            self.disk_gib.to_string(),
            "--update-connection=false".to_owned(),
            self.name.clone(),
        ]);
        all
    }

    /// `podman machine start`, without changing the default connection.
    pub fn start_args(&self) -> Vec<String> {
        let mut all = args([
            "machine",
            "start",
            "--no-info",
            "--quiet",
            "--update-connection=false",
        ]);
        all.push(self.name.clone());
        all
    }
}

/// A machine as `podman machine list` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MachineListing {
    pub name: String,
    pub running: bool,
}

/// Where dagq's machine is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MachineState {
    Missing,
    Stopped,
    Running,
}

/// dagq's machine's state and the other machines that run (which keep it
/// from starting).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MachineStatus {
    pub name: String,
    pub state: MachineState,
    pub others_running: Vec<String>,
}

/// Every machine podman knows, from `podman machine list --format json`.
pub fn machines(podman: &dyn Podman) -> BrokerResult<Vec<MachineListing>> {
    let output = checked(
        podman,
        &args(["machine", "list", "--format", "json"]),
        FailureCode::PodmanFailed,
        "podman machine list",
    )?;
    parse_machines(&output.stdout)
}

fn parse_machines(stdout: &str) -> BrokerResult<Vec<MachineListing>> {
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Listed {
        name: String,
        #[serde(default)]
        running: bool,
        #[serde(default)]
        starting: bool,
    }
    let text = stdout.trim();
    if text.is_empty() {
        return Ok(Vec::new());
    }
    let listed: Vec<Listed> = serde_json::from_str(text).map_err(|error| {
        BrokerFailure::new(
            FailureCode::PodmanFailed,
            format!("podman machine list printed what dagq cannot read: {error}"),
        )
    })?;
    Ok(listed
        .into_iter()
        .map(|machine| MachineListing {
            // The table marks the default machine with `*`; strip it in
            // case a podman does so in JSON too.
            name: machine.name.trim_end_matches('*').to_owned(),
            running: machine.running || machine.starting,
        })
        .collect())
}

/// The state of the machine `name`.
pub fn machine_status(podman: &dyn Podman, name: &str) -> BrokerResult<MachineStatus> {
    let listed = machines(podman)?;
    let state = match listed.iter().find(|machine| machine.name == name) {
        None => MachineState::Missing,
        Some(machine) if machine.running => MachineState::Running,
        Some(_) => MachineState::Stopped,
    };
    Ok(MachineStatus {
        name: name.to_owned(),
        state,
        others_running: listed
            .into_iter()
            .filter(|machine| machine.running && machine.name != name)
            .map(|machine| machine.name)
            .collect(),
    })
}

/// What [`ensure_machine`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct MachineOutcome {
    pub initialized: bool,
    pub started: bool,
}

/// Make dagq's machine exist and run, under the host lock (ADR-t827-3
/// decisions 4 and 5): init it with `spec`'s resources when missing, start
/// it when stopped, nothing when it runs. When another machine runs,
/// `machine_busy` without starting (or stopping) anything.
pub fn ensure_machine(
    podman: &dyn Podman,
    lock: &dyn HostLock,
    spec: &MachineSpec,
) -> BrokerResult<MachineOutcome> {
    let _held = lock.hold()?;
    ensure_machine_held(podman, spec)
}

/// [`ensure_machine`] for a caller that holds the host lock.
fn ensure_machine_held(podman: &dyn Podman, spec: &MachineSpec) -> BrokerResult<MachineOutcome> {
    let mut outcome = MachineOutcome::default();
    let mut status = machine_status(podman, &spec.name)?;
    if status.state == MachineState::Missing {
        checked(
            podman,
            &spec.init_args(),
            FailureCode::MachineFailed,
            &format!("podman machine init {}", spec.name),
        )?;
        outcome.initialized = true;
        status = machine_status(podman, &spec.name)?;
    }
    match status.state {
        MachineState::Running => Ok(outcome),
        MachineState::Missing => Err(BrokerFailure::new(
            FailureCode::MachineFailed,
            format!(
                "podman machine init {} succeeded but the machine is not listed",
                spec.name
            ),
        )),
        MachineState::Stopped if !status.others_running.is_empty() => Err(BrokerFailure::new(
            FailureCode::MachineBusy,
            format!(
                "the machine {} cannot start while {} runs; dagq does not stop another machine",
                spec.name,
                status.others_running.join(", ")
            ),
        )),
        MachineState::Stopped => {
            checked(
                podman,
                &spec.start_args(),
                FailureCode::MachineFailed,
                &format!("podman machine start {}", spec.name),
            )?;
            outcome.started = true;
            Ok(outcome)
        }
    }
}

/// Stop dagq's machine when no container runs on it, under the host lock
/// (ADR-t827-3 decision 5). `true` when it stopped it; a missing or
/// stopped machine, or one with a running container, is left alone.
pub fn release_machine(podman: &dyn Podman, lock: &dyn HostLock, name: &str) -> BrokerResult<bool> {
    let _held = lock.hold()?;
    if machine_status(podman, name)?.state != MachineState::Running {
        return Ok(false);
    }
    let running = checked(
        podman,
        &on_machine(name, &["ps", "--quiet"]),
        FailureCode::PodmanFailed,
        "podman ps",
    )?;
    if !running.stdout.trim().is_empty() {
        return Ok(false);
    }
    checked(
        podman,
        &args(["machine", "stop", name]),
        FailureCode::MachineFailed,
        &format!("podman machine stop {name}"),
    )?;
    Ok(true)
}

// ---------------------------------------------------------------------------
// The image.

/// How many hex digits of the material's hash a `.dirty` build's tag
/// carries.
pub const MATERIAL_HASH_DIGITS: usize = 12;

/// The image's tag for dagq's build identifier `build` whose embedded
/// material has the SHA-256 `material` (hex): `+` and `.` become `-`
/// (`0.4.0-dev+abc` is `0.4.0-dev-abc`, a release `0.4.0` is itself), and a
/// `.dirty` build adds the first [`MATERIAL_HASH_DIGITS`] of the hash
/// (`0.4.0-dev-abc-dirty-<hash>`), so two dirty builds of one commit with
/// other material do not share a tag.
pub fn image_tag(build: &str, material: &str) -> String {
    let tag = match build.split_once('+') {
        Some((version, metadata)) => format!("{version}-{}", metadata.replace('.', "-")),
        None => build.to_owned(),
    };
    if build.ends_with(".dirty") {
        let hash = material.get(..MATERIAL_HASH_DIGITS).unwrap_or(material);
        format!("{tag}-{hash}")
    } else {
        tag
    }
}

/// `localhost/dagq-broker:<tag>`.
pub fn image_name(build: &str, material: &str) -> String {
    format!("{IMAGE_REPOSITORY}:{}", image_tag(build, material))
}

/// Where the image's build context comes from: it puts the context (the
/// Containerfile and the broker's sources, or the Containerfile alone for
/// a release) in a dir and names the Rust version of the build stage.
pub trait ImageSource {
    fn stage(&self, dir: &Path) -> BrokerResult<String>;
}

/// `podman build` of `image` for dagq's build `build` from the context in
/// `context`.
pub fn build_args(
    machine: &str,
    image: &str,
    build: &str,
    rust_version: &str,
    context: &Path,
) -> Vec<String> {
    let mut all = on_machine(machine, &["build"]);
    all.extend([
        "--build-arg".to_owned(),
        format!("RUST_VERSION={rust_version}"),
        "--build-arg".to_owned(),
        format!("CARGO_BUILD_JOBS={BUILD_JOBS}"),
        "--build-arg".to_owned(),
        format!("{IMAGE_BUILD_ARG}={build}"),
        "--tag".to_owned(),
        image.to_owned(),
        "--file".to_owned(),
        context.join("Containerfile").display().to_string(),
        context.display().to_string(),
    ]);
    all
}

/// Whether the machine has `image`.
pub fn image_exists(podman: &dyn Podman, machine: &str, image: &str) -> BrokerResult<bool> {
    exists(
        podman,
        &on_machine(machine, &["image", "exists", image]),
        "podman image exists",
    )
}

/// Build `image` of dagq's build `build` unless the machine has it; `true`
/// when it built.
pub fn ensure_image(
    podman: &dyn Podman,
    machine: &str,
    image: &str,
    build: &str,
    source: &dyn ImageSource,
    scratch: &Path,
) -> BrokerResult<bool> {
    if image_exists(podman, machine, image)? {
        return Ok(false);
    }
    build_image(podman, machine, image, build, source, scratch)?;
    Ok(true)
}

/// Stage the build context in `scratch` and build `image` from it.
fn build_image(
    podman: &dyn Podman,
    machine: &str,
    image: &str,
    build: &str,
    source: &dyn ImageSource,
    scratch: &Path,
) -> BrokerResult<()> {
    let rust_version = source.stage(scratch)?;
    checked(
        podman,
        &build_args(machine, image, build, &rust_version, scratch),
        FailureCode::ImageBuildFailed,
        &format!("podman build {image}"),
    )?;
    Ok(())
}

/// What [`prune_images`] did: the old images of [`IMAGE_REPOSITORY`] it
/// removed, and the ones it could not (which do not fail the broker's
/// start).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ImagePrune {
    /// The images (`localhost/dagq-broker:<tag>`) removed.
    pub removed: Vec<String>,
    /// The images kept although they are neither the current nor the
    /// previous one, because a container on the machine uses them (the one
    /// kept for the runs that hold tokens, `kept_stale`, or another
    /// queue's).
    pub in_use: Vec<String>,
    /// The images whose `podman image rm` failed, with its error.
    pub failed: Vec<ImagePruneFailure>,
    /// The images or containers could not be listed, so nothing was
    /// removed.
    pub error: Option<String>,
}

/// An image [`prune_images`] could not remove.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImagePruneFailure {
    pub image: String,
    pub error: String,
}

/// One tag of [`IMAGE_REPOSITORY`] on the machine.
struct TaggedImage {
    name: String,
    id: String,
    created: i64,
}

/// The tags of [`IMAGE_REPOSITORY`] on the machine, from `podman images`.
/// Only names of that repository are read, so another repository's image
/// (or another name of the same image) is never named for removal.
fn broker_images(podman: &dyn Podman, machine: &str) -> BrokerResult<Vec<TaggedImage>> {
    #[derive(Deserialize)]
    struct Listed {
        #[serde(rename = "Id", default)]
        id: String,
        #[serde(rename = "Names", default)]
        names: Option<Vec<String>>,
        #[serde(rename = "Created", default)]
        created: i64,
    }
    let output = checked(
        podman,
        &on_machine(machine, &["images", "--format", "json"]),
        FailureCode::PodmanFailed,
        "podman images",
    )?;
    let listed: Vec<Listed> = serde_json::from_str(output.stdout.trim()).map_err(|error| {
        BrokerFailure::new(
            FailureCode::PodmanFailed,
            format!("podman images printed what dagq cannot read: {error}"),
        )
    })?;
    let prefix = format!("{IMAGE_REPOSITORY}:");
    Ok(listed
        .into_iter()
        .flat_map(|image| {
            let (id, created) = (image.id, image.created);
            image
                .names
                .unwrap_or_default()
                .into_iter()
                .filter(|name| name.starts_with(&prefix))
                .map(move |name| TaggedImage {
                    name,
                    id: id.clone(),
                    created,
                })
        })
        .collect())
}

/// The images (names and ids) the containers on the machine use, from
/// `podman ps --all`.
fn container_images(podman: &dyn Podman, machine: &str) -> BrokerResult<Vec<String>> {
    #[derive(Deserialize)]
    struct Listed {
        #[serde(rename = "Image", default)]
        image: String,
        #[serde(rename = "ImageID", default)]
        image_id: String,
    }
    let output = checked(
        podman,
        &on_machine(machine, &["ps", "--all", "--format", "json"]),
        FailureCode::PodmanFailed,
        "podman ps",
    )?;
    let listed: Vec<Listed> = serde_json::from_str(output.stdout.trim()).map_err(|error| {
        BrokerFailure::new(
            FailureCode::PodmanFailed,
            format!("podman ps printed what dagq cannot read: {error}"),
        )
    })?;
    Ok(listed
        .into_iter()
        .flat_map(|container| [container.image, container.image_id])
        .filter(|word| !word.is_empty())
        .collect())
}

/// Remove the old images of [`IMAGE_REPOSITORY`] on dagq's machine
/// (ADR-t827-1, ADR-t827-3 decision 2): `current` stays, and so does the
/// newest other one by its creation time (the previous; tags carry no
/// order), and any a container on the machine uses (the one kept for the
/// runs that hold tokens, or another queue's). The rest are removed by
/// name with `podman image rm` without `--force`, which untags a name the
/// image shares with another tag. Nothing here fails: what could not be
/// listed or removed is in the result.
pub fn prune_images(podman: &dyn Podman, machine: &str, current: &str) -> ImagePrune {
    let mut prune = ImagePrune::default();
    let listed = broker_images(podman, machine)
        .and_then(|images| Ok((images, container_images(podman, machine)?)));
    let (mut images, used) = match listed {
        Ok(listed) => listed,
        Err(failure) => {
            prune.error = Some(failure.message);
            return prune;
        }
    };
    // The current image under any other tag of it is the current, not the
    // previous.
    let current_ids: Vec<String> = images
        .iter()
        .filter(|image| image.name == current && !image.id.is_empty())
        .map(|image| image.id.clone())
        .collect();
    images.retain(|image| image.name != current && !current_ids.contains(&image.id));
    // Newest first; the name breaks a tie so the choice does not depend on
    // podman's order.
    images.sort_by(|a, b| b.created.cmp(&a.created).then(b.name.cmp(&a.name)));
    for image in images.into_iter().skip(1) {
        if used
            .iter()
            .any(|word| *word == image.name || (!image.id.is_empty() && *word == image.id))
        {
            prune.in_use.push(image.name);
            continue;
        }
        match podman.run(&on_machine(machine, &["image", "rm", &image.name])) {
            Ok(output) if output.success => prune.removed.push(image.name),
            Ok(output) => prune.failed.push(ImagePruneFailure {
                image: image.name,
                error: output.words().to_owned(),
            }),
            Err(failure) => prune.failed.push(ImagePruneFailure {
                image: image.name,
                error: failure.message,
            }),
        }
    }
    prune
}

// ---------------------------------------------------------------------------
// The container.

/// The resource limits of the container (ADR-t827-3 decision 7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ContainerLimits {
    pub memory: String,
    pub cpus: String,
    pub pids: u32,
}

impl Default for ContainerLimits {
    fn default() -> Self {
        Self {
            memory: "512m".to_owned(),
            cpus: "1".to_owned(),
            pids: 256,
        }
    }
}

impl ContainerLimits {
    /// The defaults with the host's overrides of `host.toml`'s `[broker]`.
    pub fn with_host(host: &crate::domain::broker::HostBroker) -> Self {
        let default = Self::default();
        Self {
            memory: host.container_memory.clone().unwrap_or(default.memory),
            cpus: host.container_cpus.clone().unwrap_or(default.cpus),
            pids: host.container_pids.unwrap_or(default.pids),
        }
    }
}

/// One bind mount: the same absolute path on the host and in the
/// container (ADR-t827-2 decision 5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Mount {
    pub path: PathBuf,
    pub read_only: bool,
}

impl Mount {
    fn arg(&self) -> String {
        let path = self.path.display();
        let mode = if self.read_only { "ro" } else { "rw" };
        format!("{path}:{path}:{mode}")
    }
}

/// The queue's broker container.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ContainerSpec {
    pub machine: String,
    pub name: String,
    pub image: String,
    /// The port on the host's `127.0.0.1`.
    pub host_port: u16,
    pub queue_dir: PathBuf,
    pub runs_dir: PathBuf,
    /// The repository's Git common dir (`git rev-parse --git-common-dir`).
    pub git_common_dir: PathBuf,
    pub limits: ContainerLimits,
    /// The flags of `serve` from `[broker]` of `dagq.toml` that differ
    /// from its defaults ([`crate::domain::broker::BrokerConfig::serve_args`]).
    pub serve_limits: Vec<String>,
}

/// `<queue dir>/broker`.
pub fn broker_dir(queue_dir: &Path) -> PathBuf {
    queue_dir.join("broker")
}

/// `dagq-broker-<queue hash>`.
pub fn container_name(queue_hash: &str) -> String {
    format!("{CONTAINER_PREFIX}{queue_hash}")
}

impl ContainerSpec {
    pub fn key(&self) -> PathBuf {
        broker_dir(&self.queue_dir).join("key")
    }

    pub fn active(&self) -> PathBuf {
        broker_dir(&self.queue_dir).join("active")
    }

    pub fn audit(&self) -> PathBuf {
        broker_dir(&self.queue_dir).join("audit")
    }

    /// Everything the container mounts, and nothing else: the runs, the
    /// Git common dir with its `config` and `hooks` read-only over it, the
    /// key and the active marks read-only, the audit. Never `$HOME`,
    /// `~/.ssh`, `~/.aws`, a Podman or Docker socket or the queue DB.
    pub fn mounts(&self) -> Vec<Mount> {
        let mount = |path: PathBuf, read_only| Mount { path, read_only };
        vec![
            mount(self.runs_dir.clone(), false),
            mount(self.git_common_dir.clone(), false),
            mount(self.git_common_dir.join("config"), true),
            mount(self.git_common_dir.join("hooks"), true),
            mount(self.key(), true),
            mount(self.active(), true),
            mount(self.audit(), false),
        ]
    }

    /// `dagq-broker serve`'s arguments inside the container.
    pub fn serve_args(&self) -> Vec<String> {
        let path = |path: PathBuf| path.display().to_string();
        let mut all = vec![
            "serve".to_owned(),
            "--container".to_owned(),
            "--listen".to_owned(),
            format!("{CONTAINER_LISTEN}:{CONTAINER_PORT}"),
            "--key".to_owned(),
            path(self.key()),
            "--active".to_owned(),
            path(self.active()),
            "--audit".to_owned(),
            path(self.audit()),
            "--root".to_owned(),
            path(self.runs_dir.clone()),
        ];
        all.extend(self.serve_limits.iter().cloned());
        all
    }

    /// The SHA-256 of the arguments of `podman run` but the label itself:
    /// the same for the same port, mounts, limits, image and serve's
    /// arguments.
    pub fn fingerprint(&self) -> String {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(self.run_args_without_label().join("\0").as_bytes());
        digest.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    /// `podman run` with [`SPEC_LABEL`] before the image (what follows the
    /// image is the entrypoint's arguments): see
    /// [`Self::run_args_without_label`].
    pub fn run_args(&self) -> Vec<String> {
        let mut all = self.run_args_without_label();
        let image = all
            .iter()
            .rposition(|arg| *arg == self.image)
            .unwrap_or(all.len());
        all.splice(
            image..image,
            [
                "--label".to_owned(),
                format!("{SPEC_LABEL}={}", self.fingerprint()),
            ],
        );
        all
    }

    /// `podman run`: detached, non-root in the host user's uid, a
    /// read-only root, every capability dropped, no privilege escalation,
    /// the limits, the port published on `127.0.0.1` only, the mounts, and
    /// no environment (no `--env`, `--env-host` or `--env-file`, so no
    /// upstream credential).
    fn run_args_without_label(&self) -> Vec<String> {
        let mut all = on_machine(
            &self.machine,
            &[
                "run",
                "--detach",
                "--name",
                &self.name,
                "--userns=keep-id",
                "--read-only",
                "--tmpfs",
                "/tmp:size=64m",
                "--cap-drop=all",
                "--security-opt",
                "no-new-privileges",
            ],
        );
        all.extend([
            "--memory".to_owned(),
            self.limits.memory.clone(),
            "--cpus".to_owned(),
            self.limits.cpus.clone(),
            "--pids-limit".to_owned(),
            self.limits.pids.to_string(),
            "--publish".to_owned(),
            format!("127.0.0.1:{}:{CONTAINER_PORT}", self.host_port),
        ]);
        for mount in self.mounts() {
            all.extend(["--volume".to_owned(), mount.arg()]);
        }
        all.push(self.image.clone());
        all.extend(self.serve_args());
        all
    }
}

/// The container as podman reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ContainerStatus {
    pub running: bool,
    pub image: String,
    /// The container's [`SPEC_LABEL`], when it has one.
    pub spec: Option<String>,
}

/// The label that carries [`ContainerSpec::fingerprint`], so a container
/// made with other arguments (another port, other mounts) is made again.
pub const SPEC_LABEL: &str = "dagq.broker.spec";

/// The container `name` on the machine, or `None` when there is none.
pub fn container_status(
    podman: &dyn Podman,
    machine: &str,
    name: &str,
) -> BrokerResult<Option<ContainerStatus>> {
    if !exists(
        podman,
        &on_machine(machine, &["container", "exists", name]),
        "podman container exists",
    )? {
        return Ok(None);
    }
    let output = checked(
        podman,
        &on_machine(machine, &["container", "inspect", "--format", "json", name]),
        FailureCode::PodmanFailed,
        &format!("podman container inspect {name}"),
    )?;
    #[derive(Deserialize)]
    struct State {
        #[serde(rename = "Running", default)]
        running: bool,
    }
    #[derive(Deserialize, Default)]
    struct Config {
        #[serde(rename = "Labels", default)]
        labels: Option<std::collections::BTreeMap<String, String>>,
    }
    #[derive(Deserialize)]
    struct Inspected {
        #[serde(rename = "State")]
        state: State,
        #[serde(rename = "Config", default)]
        config: Config,
        #[serde(rename = "ImageName", default)]
        image: String,
    }
    let inspected: Vec<Inspected> =
        serde_json::from_str(output.stdout.trim()).map_err(|error| {
            BrokerFailure::new(
                FailureCode::PodmanFailed,
                format!("podman container inspect printed what dagq cannot read: {error}"),
            )
        })?;
    Ok(inspected
        .into_iter()
        .next()
        .map(|inspected| ContainerStatus {
            running: inspected.state.running,
            image: inspected.image,
            spec: inspected
                .config
                .labels
                .and_then(|labels| labels.get(SPEC_LABEL).cloned()),
        }))
}

/// What [`ensure_container`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct ContainerOutcome {
    /// A container was made with `podman run`.
    pub created: bool,
    /// A stopped container, or one of another image, was removed first.
    pub replaced: bool,
    /// A running container of another image was kept because runs still
    /// hold tokens for it (ADR-t827-3 decision 2).
    pub kept_stale: bool,
}

/// Make the queue's container run `spec`: a running container of the same
/// image and arguments ([`SPEC_LABEL`]) stays; a running one of another
/// image or other arguments stays while `in_use` (runs hold tokens for it)
/// and is replaced otherwise; a stopped one is removed and made again, so
/// it always has the current mounts and port.
pub fn ensure_container(
    podman: &dyn Podman,
    spec: &ContainerSpec,
    in_use: bool,
) -> BrokerResult<ContainerOutcome> {
    let mut outcome = ContainerOutcome::default();
    let fingerprint = spec.fingerprint();
    match container_status(podman, &spec.machine, &spec.name)? {
        Some(status)
            if status.running
                && status.image == spec.image
                && status.spec.as_deref() == Some(fingerprint.as_str()) =>
        {
            return Ok(outcome);
        }
        Some(status) if status.running && in_use => {
            outcome.kept_stale = true;
            return Ok(outcome);
        }
        Some(_) => {
            checked(
                podman,
                &on_machine(&spec.machine, &["rm", "--force", &spec.name]),
                FailureCode::ContainerFailed,
                &format!("podman rm {}", spec.name),
            )?;
            outcome.replaced = true;
        }
        None => {}
    }
    checked(
        podman,
        &spec.run_args(),
        FailureCode::ContainerFailed,
        &format!("podman run {}", spec.name),
    )?;
    outcome.created = true;
    Ok(outcome)
}

/// Stop and remove the container `name` (its state is all in its mounts,
/// and [`ensure_container`] makes a stopped one again anyway); `true` when
/// it was running.
pub fn stop_container(podman: &dyn Podman, machine: &str, name: &str) -> BrokerResult<bool> {
    let Some(status) = container_status(podman, machine, name)? else {
        return Ok(false);
    };
    checked(
        podman,
        &on_machine(machine, &["rm", "--force", "--time", "10", name]),
        FailureCode::ContainerFailed,
        &format!("podman rm {name}"),
    )?;
    Ok(status.running)
}

/// Wait until the health on `127.0.0.1:<port>` answers `ok` in the
/// protocol this dagq speaks, polling every `interval` for at most
/// `timeout`.
pub fn wait_healthy(
    probe: &dyn HealthProbe,
    port: u16,
    timeout: Duration,
    interval: Duration,
) -> BrokerResult<dagq_broker_protocol::HealthResponse> {
    let deadline = Instant::now() + timeout;
    loop {
        let last = match probe.probe(port) {
            Ok(health)
                if health.status == "ok"
                    && health.protocol == dagq_broker_protocol::PROTOCOL_VERSION =>
            {
                return Ok(health);
            }
            Ok(health) => format!(
                "the health answered status {} in protocol {}",
                health.status, health.protocol
            ),
            Err(error) => error,
        };
        if Instant::now() >= deadline {
            return Err(BrokerFailure::new(
                FailureCode::Unhealthy,
                format!(
                    "the broker on 127.0.0.1:{port} did not answer health within {}s: {last}",
                    timeout.as_secs()
                ),
            ));
        }
        std::thread::sleep(interval);
    }
}

// ---------------------------------------------------------------------------
// start, stop, status.

/// The ports [`start`], [`stop`] and [`status`] use.
pub struct Ports<'a> {
    pub podman: &'a dyn Podman,
    pub host_lock: &'a dyn HostLock,
    pub health: &'a dyn HealthProbe,
}

/// What `dagq broker start` does, for one queue.
pub struct StartRequest<'a> {
    pub machine: &'a MachineSpec,
    pub container: &'a ContainerSpec,
    /// dagq's build identifier: the image is built as it, and a broker
    /// whose health names another is not used (ADR-t827-1 decision 7).
    pub build: &'a str,
    pub source: &'a dyn ImageSource,
    /// A dir the image's build context is staged in.
    pub scratch: &'a Path,
    /// Whether runs hold tokens (active marks), which keeps a running
    /// container of another image.
    pub in_use: bool,
    pub health_timeout: Duration,
    pub health_interval: Duration,
    /// Called once the image is known to be missing, before its build
    /// starts (the supervisor records the state `building`).
    pub on_build: Option<&'a dyn Fn()>,
}

/// What [`start`] did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StartReport {
    pub machine: MachineOutcome,
    pub image: String,
    pub image_built: bool,
    /// How long the build took, when there was one.
    pub build_ms: Option<u64>,
    pub container: String,
    pub container_outcome: ContainerOutcome,
    pub port: u16,
    pub health: dagq_broker_protocol::HealthResponse,
    /// dagq's build identifier.
    pub build: String,
    /// Whether the broker's health names dagq's build. Only a container kept
    /// for the runs that hold tokens for it may not.
    pub build_matches: bool,
    /// The image under this build's tag answered another build, so it was
    /// removed with its container and built again.
    pub rebuilt: bool,
    /// The old images removed once the broker answered ([`prune_images`]).
    pub images: ImagePrune,
}

/// Make the queue's broker run and answer health: the machine, the image,
/// the container, the health, in order. Each step does nothing when its
/// part is already there, so a second call changes nothing. A broker that
/// answers another build than dagq's, while no run holds a token for it,
/// has its image built again once; a second mismatch
/// is a [`FailureCode::VersionMismatch`] (ADR-t827-1 decision 7). Once the
/// broker answers, the old images go ([`prune_images`]).
pub fn start(ports: &Ports, request: &StartRequest) -> BrokerResult<StartReport> {
    let spec = request.container;
    // The host lock is held throughout, so a `stop` or a test's
    // `release_machine` cannot stop the machine under the build or the run
    // (ADR-t827-3 decision 5).
    let _held = ports.host_lock.hold()?;
    let machine = ensure_machine_held(ports.podman, request.machine)?;
    // The image, built when missing: the supervisor is told before the
    // build starts (`building`), and the build is timed.
    let mut build_ms = None;
    let mut image = |podman: &dyn Podman| -> BrokerResult<bool> {
        if image_exists(podman, &spec.machine, &spec.image)? {
            return Ok(false);
        }
        if let Some(on_build) = request.on_build {
            on_build();
        }
        let started = Instant::now();
        build_image(
            podman,
            &spec.machine,
            &spec.image,
            request.build,
            request.source,
            request.scratch,
        )?;
        build_ms = Some(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX));
        Ok(true)
    };
    let healthy = || {
        wait_healthy(
            ports.health,
            spec.host_port,
            request.health_timeout,
            request.health_interval,
        )
    };
    let mut image_built = image(ports.podman)?;
    let mut container_outcome = ensure_container(ports.podman, spec, request.in_use)?;
    let mut health = healthy()?;
    let mut rebuilt = false;
    if health.build != request.build && !container_outcome.kept_stale && !request.in_use {
        // The image under this build's tag was built as another build (by
        // a dagq that did not pass its build to the image, or from a
        // checkout that had moved on): it goes with its container, and is
        // built again from this dagq's material.
        checked(
            ports.podman,
            &on_machine(&spec.machine, &["rm", "--force", &spec.name]),
            FailureCode::ContainerFailed,
            &format!("podman rm {}", spec.name),
        )?;
        checked(
            ports.podman,
            &on_machine(&spec.machine, &["image", "rm", "--force", &spec.image]),
            FailureCode::PodmanFailed,
            &format!("podman image rm {}", spec.image),
        )?;
        image_built = image(ports.podman)?;
        container_outcome = ensure_container(ports.podman, spec, false)?;
        container_outcome.replaced = true;
        health = healthy()?;
        rebuilt = true;
        if health.build != request.build {
            return Err(BrokerFailure::new(
                FailureCode::VersionMismatch,
                format!(
                    "the broker built as {} answers build {}, not dagq's {}: dagq does not use it",
                    spec.image, health.build, request.build
                ),
            ));
        }
    }
    let build_matches = health.build == request.build;
    let images = prune_images(ports.podman, &spec.machine, &spec.image);
    Ok(StartReport {
        machine,
        image: spec.image.clone(),
        image_built,
        build_ms,
        container: spec.name.clone(),
        container_outcome,
        port: spec.host_port,
        health,
        build: request.build.to_owned(),
        build_matches,
        rebuilt,
        images,
    })
}

/// Make the queue's container again and wait for its health: the
/// automatic repair of a broker whose health failed in a row (ADR-t827-3
/// decision 3, ADR-0047's first layer). The host lock is held throughout,
/// so the machine is not stopped under it; a machine that does not run is
/// not started here (the next [`start`] does that).
pub fn restart(
    ports: &Ports,
    spec: &ContainerSpec,
    health_timeout: Duration,
    health_interval: Duration,
) -> BrokerResult<dagq_broker_protocol::HealthResponse> {
    let _held = ports.host_lock.hold()?;
    let machine = machine_status(ports.podman, &spec.machine)?;
    if machine.state != MachineState::Running {
        return Err(BrokerFailure::new(
            if machine.others_running.is_empty() {
                FailureCode::MachineFailed
            } else {
                FailureCode::MachineBusy
            },
            format!("the machine {} does not run", spec.machine),
        ));
    }
    stop_container(ports.podman, &spec.machine, &spec.name)?;
    ensure_container(ports.podman, spec, false)?;
    wait_healthy(
        ports.health,
        spec.host_port,
        health_timeout,
        health_interval,
    )
}

/// The queue's broker as the supervisor and `down` drive it (ADR-t827-3
/// decision 2): each call is one of the steps above on the queue's own
/// machine, image and container, and records what it did in the queue's
/// `state.json`. The adapter holds the ports and the queue's settings.
pub trait BrokerControl: Send + Sync {
    /// [`start`], with the state `building` while the image builds.
    fn ensure(&self) -> BrokerResult<StartReport>;
    /// One look at the health of the running container.
    fn health(&self) -> Result<(), String>;
    /// [`restart`].
    fn restart(&self) -> BrokerResult<()>;
    /// [`stop`].
    fn stop(&self) -> BrokerResult<StopReport>;
    /// The port of a broker recorded as running dagq's build that answers
    /// its health now with that build, without any podman command: what a
    /// claim uses before this supervisor's own start of it came back (a
    /// supervisor just started, or `dagq broker start` made it ready).
    fn running_port(&self) -> Option<u16> {
        None
    }
}

/// What [`stop`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct StopReport {
    pub container_stopped: bool,
    pub machine_stopped: bool,
}

/// Stop the queue's container, then dagq's machine when no other
/// container runs on it. A missing or stopped machine is left as it is.
pub fn stop(ports: &Ports, machine: &str, container: &str) -> BrokerResult<StopReport> {
    let container_stopped = match machine_status(ports.podman, machine)?.state {
        MachineState::Running => stop_container(ports.podman, machine, container)?,
        MachineState::Missing | MachineState::Stopped => false,
    };
    let machine_stopped = release_machine(ports.podman, ports.host_lock, machine)?;
    Ok(StopReport {
        container_stopped,
        machine_stopped,
    })
}

/// The broker's state as `dagq broker status` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StatusReport {
    /// `running`, `stopped`, `unhealthy`, `machine_busy`, `machine_missing`,
    /// `machine_stopped`, or the failure's code (`podman_missing`, ...).
    pub state: String,
    pub machine: Option<MachineStatus>,
    pub image: String,
    pub image_present: Option<bool>,
    pub container: String,
    pub container_status: Option<ContainerStatus>,
    pub port: Option<u16>,
    pub health: Option<dagq_broker_protocol::HealthResponse>,
    /// dagq's build identifier, which the image's tag and the health's
    /// `build` must name.
    pub build: String,
    /// Whether the health named dagq's build; `None` without a health.
    pub build_matches: Option<bool>,
    pub error: Option<Value>,
}

/// Read the broker's state; changes nothing. A failure (no podman, say) is
/// reported in `state` and `error` rather than returned. `build` is dagq's
/// build identifier.
pub fn status(ports: &Ports, spec: &ContainerSpec, port: Option<u16>, build: &str) -> StatusReport {
    let mut report = StatusReport {
        build: build.to_owned(),
        build_matches: None,
        state: String::new(),
        machine: None,
        image: spec.image.clone(),
        image_present: None,
        container: spec.name.clone(),
        container_status: None,
        port,
        health: None,
        error: None,
    };
    if let Err(failure) = fill_status(ports, spec, port, &mut report) {
        report.state = failure.code.as_str().to_owned();
        report.error = Some(failure.to_json());
    }
    report
}

fn fill_status(
    ports: &Ports,
    spec: &ContainerSpec,
    port: Option<u16>,
    report: &mut StatusReport,
) -> BrokerResult<()> {
    let machine = machine_status(ports.podman, &spec.machine)?;
    let state = machine.state;
    let busy = !machine.others_running.is_empty();
    report.machine = Some(machine);
    match state {
        MachineState::Missing => {
            report.state = "machine_missing".to_owned();
            return Ok(());
        }
        MachineState::Stopped => {
            report.state = if busy {
                "machine_busy"
            } else {
                "machine_stopped"
            }
            .to_owned();
            return Ok(());
        }
        MachineState::Running => {}
    }
    report.image_present = Some(image_exists(ports.podman, &spec.machine, &spec.image)?);
    let container = container_status(ports.podman, &spec.machine, &spec.name)?;
    let running = container.as_ref().is_some_and(|status| status.running);
    report.container_status = container;
    if !running {
        report.state = "stopped".to_owned();
        return Ok(());
    }
    report.state = match port.map(|port| ports.health.probe(port)) {
        Some(Ok(health)) => {
            report.build_matches = Some(health.build == report.build);
            report.health = Some(health);
            "running"
        }
        Some(Err(error)) => {
            report.error = Some(BrokerFailure::new(FailureCode::Unhealthy, error).to_json());
            "unhealthy"
        }
        None => "unhealthy",
    }
    .to_owned();
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;

    use super::*;

    /// A podman that answers from a script of outputs, keyed by the start
    /// of the arguments, and records every command.
    #[derive(Default)]
    struct Script {
        calls: RefCell<Vec<Vec<String>>>,
        answers: RefCell<Vec<(Vec<String>, VecDeque<PodmanOutput>)>>,
        missing: bool,
    }

    fn ok(stdout: &str) -> PodmanOutput {
        PodmanOutput {
            success: true,
            code: Some(0),
            stdout: stdout.to_owned(),
            stderr: String::new(),
        }
    }

    fn fail(stderr: &str) -> PodmanOutput {
        PodmanOutput {
            success: false,
            code: Some(1),
            stdout: String::new(),
            stderr: stderr.to_owned(),
        }
    }

    impl Script {
        /// Answer the commands starting with `prefix` with `outputs` in
        /// turn (the last one again once they run out).
        fn on(self, prefix: &[&str], outputs: Vec<PodmanOutput>) -> Self {
            self.answers.borrow_mut().push((
                prefix.iter().map(|word| (*word).to_owned()).collect(),
                outputs.into(),
            ));
            self
        }

        fn calls(&self) -> Vec<String> {
            self.calls
                .borrow()
                .iter()
                .map(|call| call.join(" "))
                .collect()
        }

        fn called(&self, prefix: &str) -> usize {
            self.calls()
                .iter()
                .filter(|call| call.starts_with(prefix))
                .count()
        }
    }

    impl Podman for Script {
        fn run(&self, args: &[String]) -> BrokerResult<PodmanOutput> {
            if self.missing {
                return Err(BrokerFailure::new(
                    FailureCode::PodmanMissing,
                    "podman was not found",
                ));
            }
            self.calls.borrow_mut().push(args.to_vec());
            let mut answers = self.answers.borrow_mut();
            let Some((_, outputs)) = answers
                .iter_mut()
                .filter(|(prefix, _)| args.starts_with(prefix))
                .max_by_key(|(prefix, _)| prefix.len())
            else {
                return Ok(ok(""));
            };
            Ok(if outputs.len() > 1 {
                outputs.pop_front().unwrap()
            } else {
                outputs.front().cloned().unwrap_or_default()
            })
        }
    }

    #[derive(Default)]
    struct CountingLock {
        held: Cell<usize>,
        fails: bool,
    }

    impl HostLock for CountingLock {
        fn hold(&self) -> BrokerResult<Box<dyn std::any::Any>> {
            if self.fails {
                return Err(BrokerFailure::new(FailureCode::PodmanFailed, "lock"));
            }
            self.held.set(self.held.get() + 1);
            Ok(Box::new(()))
        }
    }

    struct Healthy(Result<dagq_broker_protocol::HealthResponse, String>);

    impl HealthProbe for Healthy {
        fn probe(&self, _port: u16) -> Result<dagq_broker_protocol::HealthResponse, String> {
            self.0.clone()
        }
    }

    struct Source(Cell<usize>);

    impl ImageSource for Source {
        fn stage(&self, _dir: &Path) -> BrokerResult<String> {
            self.0.set(self.0.get() + 1);
            Ok("1.98.1".to_owned())
        }
    }

    const MISSING: &str = "[]";
    const STOPPED: &str = r#"[{"Name":"dagq","Running":false,"Starting":false}]"#;
    const RUNNING: &str = r#"[{"Name":"dagq","Running":true,"Starting":false}]"#;
    const OTHER_RUNNING: &str =
        r#"[{"Name":"dagq","Running":false},{"Name":"podman-machine-default*","Running":true}]"#;

    fn list() -> [&'static str; 4] {
        ["machine", "list", "--format", "json"]
    }

    fn spec() -> ContainerSpec {
        ContainerSpec {
            machine: MACHINE.to_owned(),
            name: container_name("abc123"),
            image: image_name("0.4.0-dev+deadbeef", ""),
            host_port: 41234,
            queue_dir: PathBuf::from("/Users/me/.local/share/dagq/abc123"),
            runs_dir: PathBuf::from("/Users/me/.local/share/dagq/abc123/runs"),
            git_common_dir: PathBuf::from("/Users/me/src/repo/.git"),
            limits: ContainerLimits::default(),
            serve_limits: Vec::new(),
        }
    }

    #[test]
    fn the_machine_is_dagqs_own_with_the_fewest_resources() {
        let machine = MachineSpec::default();
        assert_eq!(machine.name, "dagq");
        assert_eq!(
            machine.init_args(),
            [
                "machine",
                "init",
                "--cpus",
                "1",
                "--memory",
                "1024",
                "--disk-size",
                "10",
                "--update-connection=false",
                "dagq"
            ]
        );
        assert_eq!(
            machine.start_args(),
            [
                "machine",
                "start",
                "--no-info",
                "--quiet",
                "--update-connection=false",
                "dagq"
            ]
        );
        // Never the default machine's name, never a volume of its own.
        let init = machine.init_args();
        assert!(
            !init
                .iter()
                .any(|arg| arg.contains("podman-machine-default"))
        );
        assert!(!init.iter().any(|arg| arg == "--volume" || arg == "-v"));
        assert!(!init.iter().any(|arg| arg == "--now" || arg == "--rootful"));
    }

    #[test]
    fn the_container_publishes_on_loopback_only_and_mounts_only_what_it_needs() {
        let spec = spec();
        let run = spec.run_args();
        assert_eq!(&run[..3], ["--connection", "dagq", "run"]);
        // The publish: one, on 127.0.0.1.
        let publishes: Vec<&String> = run
            .iter()
            .enumerate()
            .filter(|(_, arg)| {
                arg.as_str() == "--publish" || arg.as_str() == "-p" || arg.starts_with("--publish=")
            })
            .map(|(i, _)| &run[i + 1])
            .collect();
        assert_eq!(publishes, ["127.0.0.1:41234:8750"]);
        assert!(!run.iter().any(|arg| arg == "--publish-all" || arg == "-P"));
        assert!(!run.iter().any(|arg| arg.starts_with("--network")));
        // No environment at all.
        for flag in ["-e", "--env", "--env-host", "--env-file", "--env-merge"] {
            assert!(
                !run.iter()
                    .any(|arg| arg == flag || arg.starts_with(&format!("{flag}="))),
                "{flag}"
            );
        }
        // The mounts, exactly.
        let volumes: Vec<&str> = run
            .iter()
            .enumerate()
            .filter(|(_, arg)| arg.as_str() == "--volume")
            .map(|(i, _)| run[i + 1].as_str())
            .collect();
        let q = "/Users/me/.local/share/dagq/abc123";
        let g = "/Users/me/src/repo/.git";
        assert_eq!(
            volumes,
            [
                format!("{q}/runs:{q}/runs:rw"),
                format!("{g}:{g}:rw"),
                format!("{g}/config:{g}/config:ro"),
                format!("{g}/hooks:{g}/hooks:ro"),
                format!("{q}/broker/key:{q}/broker/key:ro"),
                format!("{q}/broker/active:{q}/broker/active:ro"),
                format!("{q}/broker/audit:{q}/broker/audit:rw"),
            ]
        );
        assert!(!run.iter().any(|arg| arg == "-v" || arg == "--mount"));
        for forbidden in [
            "docker.sock",
            "podman.sock",
            "/run/podman",
            "/var/run",
            ".ssh",
            ".aws",
            ".config",
            ".gitconfig",
            "queue.db",
            "host.toml",
        ] {
            assert!(
                !volumes.iter().any(|volume| volume.contains(forbidden)),
                "{forbidden}"
            );
        }
        // Not $HOME itself, nor the queue dir as a whole.
        for volume in &volumes {
            let host = volume.split(':').next().unwrap();
            assert!(host != "/Users/me" && host != q, "{volume}");
        }
        // Locked down.
        for flag in [
            "--userns=keep-id",
            "--read-only",
            "--cap-drop=all",
            "no-new-privileges",
            "--detach",
        ] {
            assert!(run.iter().any(|arg| arg == flag), "{flag}");
        }
        assert!(!run.iter().any(|arg| arg == "--privileged"));
        let after = |flag: &str| {
            let i = run.iter().position(|arg| arg == flag).unwrap();
            run[i + 1].clone()
        };
        assert_eq!(after("--memory"), "512m");
        assert_eq!(after("--cpus"), "1");
        assert_eq!(after("--pids-limit"), "256");
        assert_eq!(after("--name"), "dagq-broker-abc123");
        // The image, then serve's arguments.
        let image = run
            .iter()
            .position(|arg| arg == "localhost/dagq-broker:0.4.0-dev-deadbeef")
            .unwrap();
        assert_eq!(
            run[image + 1..],
            [
                "serve",
                "--container",
                "--listen",
                "0.0.0.0:8750",
                "--key",
                &format!("{q}/broker/key"),
                "--active",
                &format!("{q}/broker/active"),
                "--audit",
                &format!("{q}/broker/audit"),
                "--root",
                &format!("{q}/runs"),
            ]
        );
    }

    #[test]
    fn image_tags_follow_the_build_identifier() {
        let one = "0123456789abcdef".repeat(4);
        let other = "fedcba9876543210".repeat(4);
        // A clean build and a release: the build identifier alone.
        assert_eq!(image_tag("0.4.0", &one), "0.4.0");
        assert_eq!(image_tag("0.4.0", &other), "0.4.0");
        assert_eq!(image_tag("0.4.0-dev+abc", &one), "0.4.0-dev-abc");
        assert_eq!(image_tag("0.4.0-dev+abc", &other), "0.4.0-dev-abc");
        // A dirty build: the material's hash too, so other material is
        // another tag.
        assert_eq!(
            image_tag("0.4.0-dev+abc.dirty", &one),
            "0.4.0-dev-abc-dirty-0123456789ab"
        );
        assert_eq!(
            image_tag("0.4.0-dev+abc.dirty", &other),
            "0.4.0-dev-abc-dirty-fedcba987654"
        );
        assert_eq!(
            image_tag("0.4.0-dev+abc.dirty", "ab"),
            "0.4.0-dev-abc-dirty-ab"
        );
        assert_eq!(
            image_name("0.4.0-dev+abc", &one),
            "localhost/dagq-broker:0.4.0-dev-abc"
        );
        assert_eq!(container_name("h"), "dagq-broker-h");
        let build = build_args("dagq", "img", "0.4.0-dev+abc", "1.98.1", Path::new("/ctx"));
        assert_eq!(
            build,
            [
                "--connection",
                "dagq",
                "build",
                "--build-arg",
                "RUST_VERSION=1.98.1",
                "--build-arg",
                "CARGO_BUILD_JOBS=1",
                "--build-arg",
                "DAGQ_BROKER_IMAGE_BUILD=0.4.0-dev+abc",
                "--tag",
                "img",
                "--file",
                "/ctx/Containerfile",
                "/ctx"
            ]
        );
    }

    #[test]
    fn no_podman_is_a_structured_error() {
        let podman = Script {
            missing: true,
            ..Script::default()
        };
        let lock = CountingLock::default();
        let error = ensure_machine(&podman, &lock, &MachineSpec::default()).unwrap_err();
        assert_eq!(error.code, FailureCode::PodmanMissing);
        assert_eq!(error.to_json()["code"], "podman_missing");
        assert!(error.to_string().starts_with("broker podman_missing: "));
        let health = Healthy(Err("down".to_owned()));
        let ports = Ports {
            podman: &podman,
            host_lock: &lock,
            health: &health,
        };
        let report = status(&ports, &spec(), None, "b");
        assert_eq!(report.state, "podman_missing");
        assert_eq!(report.error.unwrap()["code"], "podman_missing");
        assert_eq!(
            stop(&ports, MACHINE, "c").unwrap_err().code,
            FailureCode::PodmanMissing
        );
    }

    #[test]
    fn a_missing_machine_is_inited_then_started_and_a_second_call_does_nothing() {
        let podman = Script::default().on(&list(), vec![ok(MISSING), ok(STOPPED), ok(RUNNING)]);
        let lock = CountingLock::default();
        let spec = MachineSpec::default();
        let outcome = ensure_machine(&podman, &lock, &spec).unwrap();
        assert_eq!(
            outcome,
            MachineOutcome {
                initialized: true,
                started: true
            }
        );
        assert_eq!(podman.called("machine init"), 1);
        assert!(podman.calls().contains(&spec.init_args().join(" ")));
        assert_eq!(podman.called("machine start"), 1);
        // Again: it runs now, so nothing.
        let outcome = ensure_machine(&podman, &lock, &spec).unwrap();
        assert_eq!(outcome, MachineOutcome::default());
        assert_eq!(podman.called("machine init"), 1);
        assert_eq!(podman.called("machine start"), 1);
        assert_eq!(lock.held.get(), 2);
        // Never the default connection.
        assert_eq!(podman.called("system connection"), 0);
        assert_eq!(podman.called("machine set"), 0);
    }

    #[test]
    fn a_stopped_machine_is_started_and_a_running_one_is_left() {
        let podman = Script::default().on(&list(), vec![ok(STOPPED), ok(RUNNING)]);
        let lock = CountingLock::default();
        let spec = MachineSpec::default();
        assert_eq!(
            ensure_machine(&podman, &lock, &spec).unwrap(),
            MachineOutcome {
                initialized: false,
                started: true
            }
        );
        assert_eq!(
            ensure_machine(&podman, &lock, &spec).unwrap(),
            MachineOutcome::default()
        );
        assert_eq!(podman.called("machine init"), 0);
        assert_eq!(podman.called("machine start"), 1);

        let podman = Script::default().on(&list(), vec![ok(RUNNING)]);
        for _ in 0..2 {
            assert_eq!(
                ensure_machine(&podman, &lock, &spec).unwrap(),
                MachineOutcome::default()
            );
        }
        assert_eq!(podman.calls(), [list().join(" "), list().join(" ")]);
    }

    #[test]
    fn another_running_machine_is_busy_and_left_running() {
        let podman = Script::default().on(&list(), vec![ok(OTHER_RUNNING)]);
        let lock = CountingLock::default();
        let error = ensure_machine(&podman, &lock, &MachineSpec::default()).unwrap_err();
        assert_eq!(error.code, FailureCode::MachineBusy);
        assert!(error.message.contains("podman-machine-default"), "{error}");
        assert_eq!(podman.called("machine start"), 0);
        assert_eq!(podman.called("machine stop"), 0);
        let status = machine_status(&podman, MACHINE).unwrap();
        assert_eq!(status.state, MachineState::Stopped);
        assert_eq!(status.others_running, ["podman-machine-default"]);
    }

    #[test]
    fn machine_failures_are_structured() {
        let lock = CountingLock::default();
        let spec = MachineSpec::default();
        let podman = Script::default()
            .on(&list(), vec![ok(MISSING)])
            .on(&["machine", "init"], vec![fail("no space left")]);
        let error = ensure_machine(&podman, &lock, &spec).unwrap_err();
        assert_eq!(error.code, FailureCode::MachineFailed);
        assert!(error.message.contains("no space left"));
        // Init reported success but nothing is listed.
        let podman = Script::default().on(&list(), vec![ok(MISSING)]);
        assert_eq!(
            ensure_machine(&podman, &lock, &spec).unwrap_err().code,
            FailureCode::MachineFailed
        );
        let podman = Script::default()
            .on(&list(), vec![ok(STOPPED)])
            .on(&["machine", "start"], vec![fail("vfkit crashed")]);
        assert_eq!(
            ensure_machine(&podman, &lock, &spec).unwrap_err().code,
            FailureCode::MachineFailed
        );
        let podman = Script::default().on(&list(), vec![ok("not json")]);
        assert_eq!(
            ensure_machine(&podman, &lock, &spec).unwrap_err().code,
            FailureCode::PodmanFailed
        );
        let podman = Script::default().on(&list(), vec![fail("boom")]);
        assert_eq!(
            machine_status(&podman, MACHINE).unwrap_err().code,
            FailureCode::PodmanFailed
        );
        // A lock that cannot be held runs no podman at all.
        let failing = CountingLock {
            fails: true,
            ..CountingLock::default()
        };
        let podman = Script::default();
        assert!(ensure_machine(&podman, &failing, &spec).is_err());
        assert!(release_machine(&podman, &failing, MACHINE).is_err());
        assert!(podman.calls().is_empty());
        // An empty listing is no machine.
        assert_eq!(parse_machines("  ").unwrap(), []);
    }

    #[test]
    fn the_machine_is_stopped_only_when_no_container_runs() {
        let lock = CountingLock::default();
        let podman = Script::default()
            .on(&list(), vec![ok(RUNNING)])
            .on(&["--connection", "dagq", "ps"], vec![ok("abc\n")]);
        assert!(!release_machine(&podman, &lock, MACHINE).unwrap());
        assert_eq!(podman.called("machine stop"), 0);

        let podman = Script::default()
            .on(&list(), vec![ok(RUNNING), ok(STOPPED)])
            .on(&["--connection", "dagq", "ps"], vec![ok("")]);
        assert!(release_machine(&podman, &lock, MACHINE).unwrap());
        assert!(!release_machine(&podman, &lock, MACHINE).unwrap());
        assert_eq!(podman.called("machine stop dagq"), 1);

        for listing in [MISSING, STOPPED] {
            let podman = Script::default().on(&list(), vec![ok(listing)]);
            assert!(!release_machine(&podman, &lock, MACHINE).unwrap());
            assert_eq!(podman.called("machine stop"), 0);
        }
        let podman = Script::default()
            .on(&list(), vec![ok(RUNNING)])
            .on(&["machine", "stop"], vec![fail("busy")]);
        assert_eq!(
            release_machine(&podman, &lock, MACHINE).unwrap_err().code,
            FailureCode::MachineFailed
        );
    }

    /// A container of `image`, labelled with [`spec`]'s fingerprint.
    fn inspect(running: bool, image: &str) -> PodmanOutput {
        inspect_with(running, image, &spec().fingerprint())
    }

    fn inspect_with(running: bool, image: &str, fingerprint: &str) -> PodmanOutput {
        ok(&format!(
            r#"[{{"State":{{"Running":{running}}},"ImageName":"{image}","Config":{{"Labels":{{"{SPEC_LABEL}":"{fingerprint}"}}}}}}]"#
        ))
    }

    fn container(exists: bool, inspected: PodmanOutput) -> Script {
        Script::default()
            .on(
                &["--connection", "dagq", "container", "exists"],
                vec![if exists { ok("") } else { fail("") }],
            )
            .on(
                &["--connection", "dagq", "container", "inspect"],
                vec![inspected],
            )
    }

    #[test]
    fn the_container_is_made_once_and_kept_while_it_runs_its_image() {
        let spec = spec();
        let podman = container(false, ok("[]"));
        let outcome = ensure_container(&podman, &spec, false).unwrap();
        assert!(outcome.created && !outcome.replaced);
        assert!(podman.calls().contains(&spec.run_args().join(" ")));

        let podman = container(true, inspect(true, &spec.image));
        assert_eq!(
            ensure_container(&podman, &spec, false).unwrap(),
            ContainerOutcome::default()
        );
        assert_eq!(podman.called("--connection dagq run"), 0);

        // Stopped: removed and made again with the current arguments.
        let podman = container(true, inspect(false, &spec.image));
        let outcome = ensure_container(&podman, &spec, false).unwrap();
        assert!(outcome.created && outcome.replaced);
        assert_eq!(
            podman.called("--connection dagq rm --force dagq-broker-abc123"),
            1
        );

        // Another image: kept while in use, replaced otherwise.
        let podman = container(true, inspect(true, "localhost/dagq-broker:old"));
        let outcome = ensure_container(&podman, &spec, true).unwrap();
        assert!(outcome.kept_stale && !outcome.created);
        let outcome = ensure_container(&podman, &spec, false).unwrap();
        assert!(outcome.replaced && outcome.created);

        let podman = container(false, ok("")).on(
            &["--connection", "dagq", "run"],
            vec![fail("port is already allocated")],
        );
        let error = ensure_container(&podman, &spec, false).unwrap_err();
        assert_eq!(error.code, FailureCode::ContainerFailed);
        assert!(error.message.contains("port is already allocated"));
        // Other arguments (another port): replaced unless in use.
        let podman = container(true, inspect_with(true, &spec.image, "other"));
        let outcome = ensure_container(&podman, &spec, false).unwrap();
        assert!(outcome.replaced && outcome.created);
        let outcome = ensure_container(&podman, &spec, true).unwrap();
        assert!(outcome.kept_stale && !outcome.created);
        let mut moved = spec.clone();
        moved.host_port += 1;
        assert_ne!(moved.fingerprint(), spec.fingerprint());
        assert_eq!(spec.fingerprint(), spec.clone().fingerprint());
        let run = spec.run_args();
        let label = run.iter().position(|arg| arg == "--label").unwrap();
        assert_eq!(
            run[label + 1],
            format!("dagq.broker.spec={}", spec.fingerprint())
        );
        assert_eq!(run[label + 2], spec.image);
        // An `exists` that neither says yes (0) nor no (1) is an error, not
        // "absent".
        let mut broken = fail("cannot connect to the machine");
        broken.code = Some(125);
        let podman = Script::default().on(
            &["--connection", "dagq", "container", "exists"],
            vec![broken.clone()],
        );
        assert_eq!(
            ensure_container(&podman, &spec, false).unwrap_err().code,
            FailureCode::PodmanFailed
        );
        assert_eq!(podman.called("--connection dagq run"), 0);
        let podman =
            Script::default().on(&["--connection", "dagq", "image", "exists"], vec![broken]);
        assert_eq!(
            image_exists(&podman, MACHINE, "img").unwrap_err().code,
            FailureCode::PodmanFailed
        );
        let podman = container(true, ok("garbage"));
        assert_eq!(
            ensure_container(&podman, &spec, false).unwrap_err().code,
            FailureCode::PodmanFailed
        );
    }

    #[test]
    fn the_image_is_built_only_when_missing() {
        let source = Source(Cell::new(0));
        let dir = Path::new("/scratch");
        let podman =
            Script::default().on(&["--connection", "dagq", "image", "exists"], vec![ok("")]);
        assert!(!ensure_image(&podman, MACHINE, "img", "b", &source, dir).unwrap());
        assert_eq!(source.0.get(), 0);
        assert_eq!(podman.called("--connection dagq build"), 0);

        let podman =
            Script::default().on(&["--connection", "dagq", "image", "exists"], vec![fail("")]);
        assert!(ensure_image(&podman, MACHINE, "img", "b", &source, dir).unwrap());
        assert_eq!(source.0.get(), 1);
        assert!(
            podman
                .calls()
                .contains(&build_args(MACHINE, "img", "b", "1.98.1", dir).join(" "))
        );

        let podman = Script::default()
            .on(&["--connection", "dagq", "image", "exists"], vec![fail("")])
            .on(&["--connection", "dagq", "build"], vec![fail("oom")]);
        assert_eq!(
            ensure_image(&podman, MACHINE, "img", "b", &source, dir)
                .unwrap_err()
                .code,
            FailureCode::ImageBuildFailed
        );
    }

    #[test]
    fn health_is_waited_for_and_a_silent_broker_is_unhealthy() {
        let health = Healthy(Ok(dagq_broker_protocol::HealthResponse::ok("b")));
        let answered = wait_healthy(&health, 1, Duration::ZERO, Duration::ZERO).unwrap();
        assert_eq!(answered.build, "b");
        let silent = Healthy(Err("connection refused".to_owned()));
        let error = wait_healthy(&silent, 7, Duration::ZERO, Duration::ZERO).unwrap_err();
        assert_eq!(error.code, FailureCode::Unhealthy);
        assert!(error.message.contains("127.0.0.1:7"));
        assert!(error.message.contains("connection refused"));
        let mut other = dagq_broker_protocol::HealthResponse::ok("b");
        other.protocol = 99;
        let error =
            wait_healthy(&Healthy(Ok(other)), 7, Duration::ZERO, Duration::ZERO).unwrap_err();
        assert!(error.message.contains("protocol 99"), "{error}");
    }

    fn start_script(machine: Vec<PodmanOutput>, image: bool, container: PodmanOutput) -> Script {
        let exists = !container.stdout.is_empty();
        Script::default()
            .on(&list(), machine)
            .on(
                &["--connection", "dagq", "image", "exists"],
                vec![if image { ok("") } else { fail("") }],
            )
            .on(
                &["--connection", "dagq", "container", "exists"],
                vec![if exists { ok("") } else { fail("") }],
            )
            .on(
                &["--connection", "dagq", "container", "inspect"],
                vec![container],
            )
    }

    #[test]
    fn start_goes_through_every_step_and_again_changes_nothing() {
        let spec = spec();
        let machine = MachineSpec::default();
        let source = Source(Cell::new(0));
        let lock = CountingLock::default();
        let health = Healthy(Ok(dagq_broker_protocol::HealthResponse::ok("b")));
        let request = StartRequest {
            machine: &machine,
            container: &spec,
            build: "b",
            source: &source,
            scratch: Path::new("/scratch"),
            in_use: false,
            health_timeout: Duration::ZERO,
            health_interval: Duration::ZERO,
            on_build: None,
        };
        // From nothing: init, start, build, run.
        let podman = start_script(
            vec![ok(MISSING), ok(STOPPED), ok(RUNNING)],
            false,
            PodmanOutput::default(),
        );
        let ports = Ports {
            podman: &podman,
            host_lock: &lock,
            health: &health,
        };
        let report = start(&ports, &request).unwrap();
        assert!(report.machine.initialized && report.machine.started);
        assert!(report.image_built && report.container_outcome.created);
        assert_eq!(report.port, 41234);
        let order: Vec<String> = podman
            .calls()
            .into_iter()
            .filter(|call| {
                call.starts_with("machine init")
                    || call.starts_with("machine start")
                    || call.starts_with("--connection dagq build")
                    || call.starts_with("--connection dagq run")
            })
            .map(|call| call.split(' ').take(3).collect::<Vec<_>>().join(" "))
            .collect();
        assert_eq!(
            order,
            [
                "machine init --cpus",
                "machine start --no-info",
                "--connection dagq build",
                "--connection dagq run"
            ]
        );
        // Everything there: nothing but reads.
        let podman = start_script(vec![ok(RUNNING)], true, inspect(true, &spec.image));
        let ports = Ports {
            podman: &podman,
            host_lock: &lock,
            health: &health,
        };
        let report = start(&ports, &request).unwrap();
        assert_eq!(report.machine, MachineOutcome::default());
        assert!(!report.image_built);
        assert_eq!(report.container_outcome, ContainerOutcome::default());
        for write in ["machine init", "machine start", "--connection dagq build"] {
            assert_eq!(podman.called(write), 0, "{write}");
        }
        assert_eq!(podman.called("--connection dagq run"), 0);
        // A broker that does not answer is an error, not a success.
        let silent = Healthy(Err("refused".to_owned()));
        let ports = Ports {
            podman: &podman,
            host_lock: &lock,
            health: &silent,
        };
        assert_eq!(
            start(&ports, &request).unwrap_err().code,
            FailureCode::Unhealthy
        );
    }

    /// A health that answers these builds in turn (the last one again).
    struct Builds(RefCell<VecDeque<&'static str>>);

    impl HealthProbe for Builds {
        fn probe(&self, _port: u16) -> Result<dagq_broker_protocol::HealthResponse, String> {
            let mut builds = self.0.borrow_mut();
            let build = if builds.len() > 1 {
                builds.pop_front().unwrap()
            } else {
                builds[0]
            };
            Ok(dagq_broker_protocol::HealthResponse::ok(build))
        }
    }

    #[test]
    fn a_broker_of_another_build_is_built_again_unless_runs_hold_it() {
        let spec = spec();
        let machine = MachineSpec::default();
        let lock = CountingLock::default();
        let source = Source(Cell::new(0));
        let request = |in_use| StartRequest {
            machine: &machine,
            container: &spec,
            build: "b",
            source: &source,
            scratch: Path::new("/scratch"),
            in_use,
            health_timeout: Duration::ZERO,
            health_interval: Duration::ZERO,
            on_build: None,
        };
        let rebuilding = || {
            Script::default()
                .on(&list(), vec![ok(RUNNING)])
                .on(
                    &["--connection", "dagq", "image", "exists"],
                    vec![ok(""), fail("")],
                )
                .on(
                    &["--connection", "dagq", "container", "exists"],
                    vec![ok(""), fail("")],
                )
                .on(
                    &["--connection", "dagq", "container", "inspect"],
                    vec![inspect(true, &spec.image)],
                )
        };
        // The image under this build's tag answers another build: it goes
        // with its container and is built again, once.
        let podman = rebuilding();
        let health = Builds(RefCell::new(["old", "b"].into()));
        let ports = Ports {
            podman: &podman,
            host_lock: &lock,
            health: &health,
        };
        let report = start(&ports, &request(false)).unwrap();
        assert!(report.rebuilt && report.build_matches, "{report:?}");
        assert!(report.image_built && report.container_outcome.created);
        assert_eq!(report.build, "b");
        assert_eq!(
            podman.called(&format!(
                "--connection dagq image rm --force {}",
                spec.image
            )),
            1
        );
        assert_eq!(
            podman.called("--connection dagq rm --force dagq-broker-abc123"),
            1
        );
        assert_eq!(podman.called("--connection dagq build"), 1);
        assert_eq!(source.0.get(), 1);

        // Another build again after that: not used.
        let podman = rebuilding();
        let health = Builds(RefCell::new(["old"].into()));
        let ports = Ports {
            podman: &podman,
            host_lock: &lock,
            health: &health,
        };
        let error = start(&ports, &request(false)).unwrap_err();
        assert_eq!(error.code, FailureCode::VersionMismatch);
        assert!(
            error.message.contains("answers build old, not dagq's b"),
            "{error}"
        );

        // A container of another image kept for the runs that hold tokens
        // for it answers its own build, and stays as it is.
        let podman = start_script(
            vec![ok(RUNNING)],
            true,
            inspect(true, "localhost/dagq-broker:old"),
        );
        let ports = Ports {
            podman: &podman,
            host_lock: &lock,
            health: &health,
        };
        let report = start(&ports, &request(true)).unwrap();
        assert!(report.container_outcome.kept_stale);
        assert!(!report.build_matches && !report.rebuilt);
        assert_eq!(podman.called("--connection dagq image rm"), 0);
        assert_eq!(podman.called("--connection dagq rm"), 0);

        // So does one of this build's tag that answers another build while
        // runs hold tokens for it.
        let podman = rebuilding();
        let ports = Ports {
            podman: &podman,
            host_lock: &lock,
            health: &health,
        };
        let report = start(&ports, &request(true)).unwrap();
        assert!(!report.build_matches && !report.rebuilt);
        assert_eq!(podman.called("--connection dagq image rm"), 0);
        assert_eq!(podman.called("--connection dagq rm"), 0);
    }

    #[test]
    fn the_client_is_reported_with_its_build_and_whether_dagq_uses_it() {
        let dir = tempfile::tempdir().unwrap();
        let dagq = dir.path().join("dagq");
        let ours = |_: &Path| Ok::<_, String>("b".to_owned());
        let report = client_report(&dagq, "b", &ours);
        assert_eq!(report.path, dir.path().join(CLIENT_BINARY));
        assert!(!report.matches && report.build.is_none());
        assert_eq!(report.error.unwrap()["code"], "client_missing");
        std::fs::write(dir.path().join(CLIENT_BINARY), "").unwrap();
        let report = client_report(&dagq, "b", &ours);
        assert!(report.matches && report.error.is_none());
        assert_eq!(report.build.as_deref(), Some("b"));
        let report = client_report(&dagq, "c", &ours);
        assert!(!report.matches);
        assert_eq!(report.build.as_deref(), Some("b"));
        assert_eq!(report.error.unwrap()["code"], "version_mismatch");
    }

    #[test]
    fn the_hosts_resources_reach_machine_init_and_podman_run() {
        use crate::domain::broker::HostBroker;
        // No override: the defaults.
        assert_eq!(
            MachineSpec::with_host(&HostBroker::default()),
            MachineSpec::default()
        );
        assert_eq!(
            ContainerLimits::with_host(&HostBroker::default()),
            ContainerLimits::default()
        );
        let host = HostBroker {
            machine_cpus: Some(2),
            machine_memory_mib: Some(2048),
            machine_disk_gib: Some(20),
            container_memory: Some("1g".into()),
            container_cpus: Some("2".into()),
            container_pids: Some(512),
            ..HostBroker::default()
        };
        let machine = MachineSpec::with_host(&host);
        assert_eq!(machine.name, MACHINE);
        assert_eq!(
            machine.init_args(),
            [
                "machine",
                "init",
                "--cpus",
                "2",
                "--memory",
                "2048",
                "--disk-size",
                "20",
                "--update-connection=false",
                "dagq"
            ]
        );
        let mut container = spec();
        container.limits = ContainerLimits::with_host(&host);
        container.serve_limits = vec!["--exec-allow".into(), "ls".into()];
        let run = container.run_args().join(" ");
        assert!(
            run.contains("--memory 1g --cpus 2 --pids-limit 512"),
            "{run}"
        );
        assert!(run.ends_with(" --exec-allow ls"), "{run}");
        // Other limits are another container.
        assert_ne!(container.fingerprint(), spec().fingerprint());
    }

    /// `podman images --format json` of these `(id, names, created)`.
    fn images(listed: &[(&str, &[&str], i64)]) -> PodmanOutput {
        let listed: Vec<Value> = listed
            .iter()
            .map(|(id, names, created)| {
                serde_json::json!({"Id": id, "Names": names, "Created": created})
            })
            .collect();
        ok(&serde_json::to_string(&listed).unwrap())
    }

    /// `podman ps --all --format json` of containers of these images.
    fn containers(images: &[&str]) -> PodmanOutput {
        let listed: Vec<Value> = images
            .iter()
            .map(|image| serde_json::json!({"Image": image, "ImageID": "id-of-another"}))
            .collect();
        ok(&serde_json::to_string(&listed).unwrap())
    }

    fn pruning(listed: PodmanOutput, used: PodmanOutput) -> Script {
        Script::default()
            .on(&["--connection", "dagq", "images"], vec![listed])
            .on(&["--connection", "dagq", "ps"], vec![used])
    }

    #[test]
    fn start_removes_all_but_the_current_and_the_previous_image() {
        let spec = spec();
        let machine = MachineSpec::default();
        let source = Source(Cell::new(0));
        let lock = CountingLock::default();
        let health = Healthy(Ok(dagq_broker_protocol::HealthResponse::ok("b")));
        let request = StartRequest {
            machine: &machine,
            container: &spec,
            build: "b",
            source: &source,
            scratch: Path::new("/scratch"),
            in_use: false,
            health_timeout: Duration::ZERO,
            health_interval: Duration::ZERO,
            on_build: None,
        };
        let listed = images(&[
            ("i-older", &["localhost/dagq-broker:older"], 100),
            ("i-current", &[spec.image.as_str()], 400),
            ("i-oldest", &["localhost/dagq-broker:oldest"], 50),
            // The previous by creation time, although its tag sorts first.
            ("i-previous", &["localhost/dagq-broker:0.1.0"], 300),
            ("i-other", &["localhost/other:old", "docker.io/x/y:1"], 1),
        ]);
        // Another tag of the current image is not the previous.
        let podman = pruning(
            images(&[
                (
                    "i-current",
                    &[spec.image.as_str(), "localhost/dagq-broker:alias"],
                    400,
                ),
                ("i-previous", &["localhost/dagq-broker:0.1.0"], 300),
                ("i-oldest", &["localhost/dagq-broker:oldest"], 50),
            ]),
            containers(&[]),
        );
        let prune = prune_images(&podman, MACHINE, &spec.image);
        assert_eq!(prune.removed, ["localhost/dagq-broker:oldest"]);
        let podman = start_script(vec![ok(RUNNING)], true, inspect(true, &spec.image))
            .on(&["--connection", "dagq", "images"], vec![listed.clone()])
            .on(
                &["--connection", "dagq", "ps"],
                vec![containers(&[&spec.image])],
            );
        let ports = Ports {
            podman: &podman,
            host_lock: &lock,
            health: &health,
        };
        let report = start(&ports, &request).unwrap();
        assert_eq!(
            report.images.removed,
            [
                "localhost/dagq-broker:older",
                "localhost/dagq-broker:oldest"
            ]
        );
        assert!(report.images.failed.is_empty() && report.images.error.is_none());
        let removals: Vec<String> = podman
            .calls()
            .into_iter()
            .filter(|call| call.contains("image rm"))
            .collect();
        assert_eq!(
            removals,
            [
                "--connection dagq image rm localhost/dagq-broker:older",
                "--connection dagq image rm localhost/dagq-broker:oldest"
            ]
        );
        // Every podman command is on dagq's machine (no machine command in
        // this start, which found it running).
        for call in podman.calls() {
            assert!(
                call.starts_with("--connection dagq ") || call.starts_with("machine list"),
                "{call}"
            );
        }

        // A container kept for the runs that hold tokens keeps its image,
        // however old; the others still go.
        let podman = pruning(
            listed.clone(),
            containers(&["localhost/dagq-broker:oldest", &spec.image]),
        );
        let prune = prune_images(&podman, MACHINE, &spec.image);
        assert_eq!(prune.removed, ["localhost/dagq-broker:older"]);
        assert_eq!(prune.in_use, ["localhost/dagq-broker:oldest"]);
        // Matched by the image's id as well.
        let podman = pruning(
            listed.clone(),
            ok(r#"[{"Image":"sha256:abc","ImageID":"i-older"}]"#),
        );
        let prune = prune_images(&podman, MACHINE, &spec.image);
        assert_eq!(prune.removed, ["localhost/dagq-broker:oldest"]);
        assert_eq!(prune.in_use, ["localhost/dagq-broker:older"]);

        // A removal that fails is reported, and the start goes on.
        let podman = start_script(vec![ok(RUNNING)], true, inspect(true, &spec.image))
            .on(&["--connection", "dagq", "images"], vec![listed.clone()])
            .on(&["--connection", "dagq", "ps"], vec![containers(&[])])
            .on(
                &[
                    "--connection",
                    "dagq",
                    "image",
                    "rm",
                    "localhost/dagq-broker:older",
                ],
                vec![fail("image is in use by a container")],
            );
        let ports = Ports {
            podman: &podman,
            host_lock: &lock,
            health: &health,
        };
        let report = start(&ports, &request).unwrap();
        assert_eq!(report.images.removed, ["localhost/dagq-broker:oldest"]);
        assert_eq!(
            report.images.failed,
            [ImagePruneFailure {
                image: "localhost/dagq-broker:older".to_owned(),
                error: "image is in use by a container".to_owned(),
            }]
        );
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(
            json["images"]["failed"][0]["image"],
            "localhost/dagq-broker:older"
        );

        // Only the current image: nothing is removed.
        let podman = pruning(
            images(&[("i-current", &[spec.image.as_str()], 400)]),
            containers(&[]),
        );
        assert_eq!(
            prune_images(&podman, MACHINE, &spec.image),
            ImagePrune::default()
        );
        assert_eq!(podman.called("--connection dagq image rm"), 0);
        // The current and one previous: nothing either.
        let podman = pruning(
            images(&[
                ("i-current", &[spec.image.as_str()], 400),
                ("i-previous", &["localhost/dagq-broker:0.1.0"], 300),
            ]),
            containers(&[]),
        );
        assert_eq!(
            prune_images(&podman, MACHINE, &spec.image),
            ImagePrune::default()
        );

        // What cannot be listed removes nothing and is reported.
        let podman = pruning(ok("garbage"), containers(&[]));
        let prune = prune_images(&podman, MACHINE, &spec.image);
        assert!(prune.error.unwrap().contains("podman images"));
        assert_eq!(podman.called("--connection dagq image rm"), 0);
        let podman = pruning(listed, fail("no machine"));
        let prune = prune_images(&podman, MACHINE, &spec.image);
        assert!(prune.error.unwrap().contains("podman ps failed"));
        assert_eq!(podman.called("--connection dagq image rm"), 0);
        let missing = Script {
            missing: true,
            ..Script::default()
        };
        assert!(prune_images(&missing, MACHINE, &spec.image).error.is_some());
    }

    #[test]
    fn start_says_when_the_image_builds() {
        let spec = spec();
        let machine = MachineSpec::default();
        let source = Source(Cell::new(0));
        let lock = CountingLock::default();
        let health = Healthy(Ok(dagq_broker_protocol::HealthResponse::ok("b")));
        let told = Cell::new(0);
        let on_build = || told.set(told.get() + 1);
        let request = StartRequest {
            machine: &machine,
            container: &spec,
            build: "b",
            source: &source,
            scratch: Path::new("/scratch"),
            in_use: false,
            health_timeout: Duration::ZERO,
            health_interval: Duration::ZERO,
            on_build: Some(&on_build),
        };
        let podman = start_script(vec![ok(RUNNING)], false, PodmanOutput::default());
        let ports = Ports {
            podman: &podman,
            host_lock: &lock,
            health: &health,
        };
        let report = start(&ports, &request).unwrap();
        assert!(report.image_built && report.build_ms.is_some());
        assert_eq!(told.get(), 1);
        assert_eq!(podman.called("--connection dagq image exists"), 1);
        // With the image there, no build and no word of one.
        let podman = start_script(vec![ok(RUNNING)], true, inspect(true, &spec.image));
        let ports = Ports {
            podman: &podman,
            host_lock: &lock,
            health: &health,
        };
        let report = start(&ports, &request).unwrap();
        assert!(!report.image_built && report.build_ms.is_none());
        assert_eq!(told.get(), 1);
    }

    #[test]
    fn restart_makes_the_container_again_on_a_running_machine_only() {
        let spec = spec();
        let lock = CountingLock::default();
        let health = Healthy(Ok(dagq_broker_protocol::HealthResponse::ok("b")));
        // The container is there, then removed.
        let podman = Script::default()
            .on(&list(), vec![ok(RUNNING)])
            .on(
                &["--connection", "dagq", "container", "exists"],
                vec![ok(""), fail("")],
            )
            .on(
                &["--connection", "dagq", "container", "inspect"],
                vec![inspect(true, &spec.image)],
            );
        let ports = Ports {
            podman: &podman,
            host_lock: &lock,
            health: &health,
        };
        restart(&ports, &spec, Duration::ZERO, Duration::ZERO).unwrap();
        assert_eq!(
            podman.called("--connection dagq rm --force --time 10 dagq-broker-abc123"),
            1
        );
        assert_eq!(podman.called("--connection dagq run --detach"), 1);
        assert_eq!(lock.held.get(), 1);
        // Still silent after it: unhealthy.
        let silent = Healthy(Err("refused".to_owned()));
        let ports = Ports {
            podman: &podman,
            host_lock: &lock,
            health: &silent,
        };
        assert_eq!(
            restart(&ports, &spec, Duration::ZERO, Duration::ZERO)
                .unwrap_err()
                .code,
            FailureCode::Unhealthy
        );
        // A machine that does not run is not started here; a person's
        // machine running is machine_busy.
        for (machines, code) in [
            (STOPPED, FailureCode::MachineFailed),
            (OTHER_RUNNING, FailureCode::MachineBusy),
        ] {
            let podman = start_script(vec![ok(machines)], true, inspect(true, &spec.image));
            let ports = Ports {
                podman: &podman,
                host_lock: &lock,
                health: &health,
            };
            assert_eq!(
                restart(&ports, &spec, Duration::ZERO, Duration::ZERO)
                    .unwrap_err()
                    .code,
                code
            );
            assert_eq!(podman.called("--connection dagq run"), 0);
            assert_eq!(podman.called("machine start"), 0);
        }
    }

    #[test]
    fn stop_stops_the_container_and_then_the_idle_machine() {
        let lock = CountingLock::default();
        let health = Healthy(Err("x".to_owned()));
        let podman = start_script(
            vec![ok(RUNNING), ok(RUNNING), ok(STOPPED)],
            true,
            inspect(true, "img"),
        )
        .on(&["--connection", "dagq", "ps"], vec![ok("")]);
        let ports = Ports {
            podman: &podman,
            host_lock: &lock,
            health: &health,
        };
        let report = stop(&ports, MACHINE, "dagq-broker-abc123").unwrap();
        assert!(report.container_stopped && report.machine_stopped);
        assert_eq!(
            podman.called("--connection dagq rm --force --time 10 dagq-broker-abc123"),
            1
        );
        // Again: the machine is stopped, so nothing.
        let report = stop(&ports, MACHINE, "dagq-broker-abc123").unwrap();
        assert_eq!(
            report,
            StopReport {
                container_stopped: false,
                machine_stopped: false
            }
        );
        assert_eq!(podman.called("machine stop"), 1);
        // A stopped container is removed but was not running.
        let podman = start_script(vec![ok(RUNNING)], true, inspect(false, "img"))
            .on(&["--connection", "dagq", "ps"], vec![ok("other\n")]);
        let ports = Ports {
            podman: &podman,
            host_lock: &lock,
            health: &health,
        };
        let report = stop(&ports, MACHINE, "c").unwrap();
        assert!(!report.container_stopped && !report.machine_stopped);
    }

    #[test]
    fn status_reports_each_state_without_changing_anything() {
        let spec = spec();
        let lock = CountingLock::default();
        let up = Healthy(Ok(dagq_broker_protocol::HealthResponse::ok("b")));
        let down = Healthy(Err("refused".to_owned()));
        let cases: Vec<(Script, &Healthy, Option<u16>, &str)> = vec![
            (
                start_script(vec![ok(MISSING)], false, PodmanOutput::default()),
                &up,
                None,
                "machine_missing",
            ),
            (
                start_script(vec![ok(STOPPED)], false, PodmanOutput::default()),
                &up,
                None,
                "machine_stopped",
            ),
            (
                start_script(vec![ok(OTHER_RUNNING)], false, PodmanOutput::default()),
                &up,
                None,
                "machine_busy",
            ),
            (
                start_script(vec![ok(RUNNING)], true, PodmanOutput::default()),
                &up,
                Some(1),
                "stopped",
            ),
            (
                start_script(vec![ok(RUNNING)], true, inspect(true, &spec.image)),
                &up,
                Some(1),
                "running",
            ),
            (
                start_script(vec![ok(RUNNING)], true, inspect(true, &spec.image)),
                &down,
                Some(1),
                "unhealthy",
            ),
            (
                start_script(vec![ok(RUNNING)], true, inspect(true, &spec.image)),
                &up,
                None,
                "unhealthy",
            ),
        ];
        for (podman, health, port, expected) in cases {
            let ports = Ports {
                podman: &podman,
                host_lock: &lock,
                health,
            };
            let report = status(&ports, &spec, port, "b");
            assert_eq!(report.state, expected);
            assert_eq!(report.build, "b");
            assert_eq!(
                report.build_matches,
                (expected == "running").then_some(true),
                "{expected}"
            );
            for write in ["machine init", "machine start", "machine stop"] {
                assert_eq!(podman.called(write), 0, "{expected} {write}");
            }
            assert_eq!(podman.called("--connection dagq run"), 0);
            assert_eq!(podman.called("--connection dagq build"), 0);
        }
        assert_eq!(lock.held.get(), 0);
    }
}
