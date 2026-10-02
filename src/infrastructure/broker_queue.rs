//! One queue's resource broker ([`QueueBroker`]): the adapter of
//! [`BrokerControl`] that `dagq broker start` and `stop`, the supervisor and
//! `down` drive (ADR-t827-3 decision 2). It holds the queue's paths, the
//! host's settings (`host.toml`'s `[broker]`) and the repository's limits
//! (`dagq.toml`'s `[broker]`), takes the queue's lock around each step,
//! and records what it did in `<queue dir>/broker/state.json`, including
//! the state `building` while the image builds.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dagq_broker_protocol::PROTOCOL_VERSION;

use crate::application::broker::{
    self as broker, BrokerControl, BrokerFailure, BrokerResult, ContainerLimits, ContainerSpec,
    FailureCode, HEALTH_TIMEOUT, HealthProbe, HostLock, ImageSource, MachineSpec, Podman,
    PodmanOutput, Reconnecting, StartReport, StopReport, container_name,
};

use super::broker_podman::{BrokerState, FileLock, free_port, tokens_active};

/// How many times a claim looks at the health of a broker recorded as
/// running before it gives the worker no tools.
const RUNNING_PORT_PROBES: usize = 3;

/// A podman that is not there: every command fails with the failure it
/// was resolved with (`podman_missing`), so the supervisor reports it as
/// the broker's state rather than failing to start.
pub struct MissingPodman(pub BrokerFailure);

impl Podman for MissingPodman {
    fn run(&self, _args: &[String]) -> BrokerResult<PodmanOutput> {
        Err(self.0.clone())
    }
}

/// The ports a [`QueueBroker`] works through.
#[derive(Clone)]
pub struct BrokerPorts {
    pub podman: Arc<dyn Podman + Send + Sync>,
    pub host_lock: Arc<dyn HostLock + Send + Sync>,
    pub health: Arc<dyn HealthProbe + Send + Sync>,
    pub source: Arc<dyn ImageSource + Send + Sync>,
}

/// The queue's broker.
#[derive(Clone)]
pub struct QueueBroker {
    pub queue_dir: PathBuf,
    pub runs_dir: PathBuf,
    pub queue_hash: String,
    /// The repository's Git common dir; `None` fails with
    /// `repository_unknown` when a container is to be made.
    pub git_common_dir: Option<PathBuf>,
    /// This binary's build identifier: the image is built as it, and a
    /// broker whose health names another is not used.
    pub build: String,
    /// The image of this build (`broker_image::image`).
    pub image: String,
    pub machine: MachineSpec,
    pub limits: ContainerLimits,
    pub serve_limits: Vec<String>,
    /// The port asked for (`--port`, `host.toml`); else the one used
    /// before, else a free one.
    pub port: Option<u16>,
    pub health_timeout: Duration,
    pub health_interval: Duration,
    pub ports: BrokerPorts,
}

/// `path` with its links resolved, or as it is when it cannot be (a dir
/// not made yet).
fn real_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}

/// A failure of dagq's own side (a lock, a file), as the step's failure.
fn local(code: FailureCode, what: &str, error: impl std::fmt::Display) -> BrokerFailure {
    BrokerFailure::new(code, format!("{what}: {error:#}"))
}

impl QueueBroker {
    /// The defaults of [`QueueBroker`]'s timing, with `ports`.
    pub fn new(
        queue_dir: PathBuf,
        runs_dir: PathBuf,
        queue_hash: String,
        git_common_dir: Option<PathBuf>,
        ports: BrokerPorts,
    ) -> Self {
        Self {
            queue_dir,
            runs_dir,
            queue_hash,
            git_common_dir,
            build: crate::VERSION.to_owned(),
            image: super::broker_image::image(),
            machine: MachineSpec::default(),
            limits: ContainerLimits::default(),
            serve_limits: Vec::new(),
            port: None,
            health_timeout: HEALTH_TIMEOUT,
            health_interval: Duration::from_millis(500),
            ports,
        }
    }

    pub fn container(&self) -> String {
        container_name(&self.queue_hash)
    }

    fn broker_ports(&self) -> broker::Ports<'_> {
        broker::Ports {
            podman: &*self.ports.podman,
            host_lock: &*self.ports.host_lock,
            health: &*self.ports.health,
        }
    }

    /// The container on `port`.
    pub fn spec(&self, port: u16) -> BrokerResult<ContainerSpec> {
        let git_common_dir = self.git_common_dir.clone().ok_or_else(|| {
            BrokerFailure::new(
                FailureCode::RepositoryUnknown,
                "run from inside the repository the queue belongs to: the broker mounts its Git common dir",
            )
        })?;
        Ok(ContainerSpec {
            machine: self.machine.name.clone(),
            name: self.container(),
            image: self.image.clone(),
            host_port: port,
            // Real paths: a run's token names its canonical workspace, and
            // the broker compares it with the roots it mounts (macOS's
            // `/var` is a link to `/private/var`).
            queue_dir: real_path(&self.queue_dir),
            runs_dir: real_path(&self.runs_dir),
            git_common_dir: real_path(&git_common_dir),
            limits: self.limits.clone(),
            serve_limits: self.serve_limits.clone(),
        })
    }

    fn lock_queue(&self) -> BrokerResult<Box<dyn std::any::Any>> {
        FileLock::queue(&self.queue_dir).hold()
    }

    fn write_state(&self, state: &BrokerState) {
        if let Err(error) = state.write(&self.queue_dir) {
            tracing::warn!(error = %format_args!("{error:#}"), "the broker's state could not be written: {error:#}");
        }
    }

    /// Make the queue's broker run: see [`broker::start`]. The state is
    /// `building` while the image builds, then `running` or the failure's
    /// code.
    pub fn start(&self) -> BrokerResult<StartReport> {
        let _queue = self.lock_queue()?;
        let mut state = BrokerState::read(&self.queue_dir);
        let port = match self.port.or(state.port) {
            Some(port) => port,
            None => free_port().map_err(|error| {
                local(
                    FailureCode::ContainerFailed,
                    "pick the broker's port",
                    error,
                )
            })?,
        };
        // The runs' dir first, so the container's paths are its real ones.
        std::fs::create_dir_all(&self.runs_dir).map_err(|error| {
            local(
                FailureCode::ContainerFailed,
                &format!("create {}", self.runs_dir.display()),
                error,
            )
        })?;
        let container = self.spec(port)?;
        // What the container mounts must exist before podman mounts it.
        crate::infrastructure::broker_token::ensure_key(&self.queue_dir)
            .map_err(|error| local(FailureCode::ContainerFailed, "make the signing key", error))?;
        for dir in [
            self.runs_dir.clone(),
            container.active(),
            container.audit(),
            container.git_common_dir.join("hooks"),
        ] {
            std::fs::create_dir_all(&dir).map_err(|error| {
                local(
                    FailureCode::ContainerFailed,
                    &format!("create {}", dir.display()),
                    error,
                )
            })?;
        }
        state.port = Some(port);
        state.container = Some(container.name.clone());
        let building = {
            let mut building = state.clone();
            building.state = Some("building".to_owned());
            building.build = Some(self.build.clone());
            building
        };
        let on_build = || self.write_state(&building);
        let scratch = broker::broker_dir(&self.queue_dir).join("build-context");
        let started = broker::start(
            &self.broker_ports(),
            &broker::StartRequest {
                machine: &self.machine,
                container: &container,
                build: &self.build,
                source: &*self.ports.source,
                scratch: &scratch,
                in_use: tokens_active(&self.queue_dir),
                health_timeout: self.health_timeout,
                health_interval: self.health_interval,
                on_build: Some(&on_build),
            },
        );
        let _ = std::fs::remove_dir_all(&scratch);
        match &started {
            // A container kept for the runs that still hold tokens runs the
            // image recorded before.
            Ok(report) if report.container_outcome.kept_stale => {}
            _ => {
                state.image = Some(container.image.clone());
                state.build = Some(self.build.clone());
            }
        }
        match &started {
            Ok(_) => {
                state.state = Some("running".to_owned());
                state.started_at = Some(unix_now());
            }
            Err(failure) => state.state = Some(failure.code.as_str().to_owned()),
        }
        self.write_state(&state);
        started
    }

    /// The broker's state as `dagq broker status` reads it: see
    /// [`broker::status`]; a queue with no repository is its failure.
    pub fn status(&self) -> BrokerResult<broker::StatusReport> {
        let port = BrokerState::read(&self.queue_dir).port;
        let container = self.spec(port.unwrap_or(0))?;
        Ok(broker::status(
            &self.broker_ports(),
            &container,
            port,
            &self.build,
        ))
    }

    /// Stop the queue's container, then dagq's machine when no container
    /// runs on it: see [`broker::stop`].
    pub fn stop(&self) -> BrokerResult<StopReport> {
        let _queue = self.lock_queue()?;
        let report = broker::stop(&self.broker_ports(), &self.machine.name, &self.container())?;
        let mut state = BrokerState::read(&self.queue_dir);
        state.container = Some(self.container());
        state.state = Some("stopped".to_owned());
        self.write_state(&state);
        Ok(report)
    }
}

impl BrokerControl for QueueBroker {
    fn ensure(&self) -> BrokerResult<StartReport> {
        self.start()
    }

    fn health(&self) -> Result<(), String> {
        let state = BrokerState::read(&self.queue_dir);
        let port = state
            .port
            .ok_or_else(|| "the broker's state names no port".to_owned())?;
        let health = self.ports.health.probe(port)?;
        if health.status == "ok" && health.protocol == PROTOCOL_VERSION {
            Ok(())
        } else {
            Err(format!(
                "the health answered status {} in protocol {}",
                health.status, health.protocol
            ))
        }
    }

    fn restart(&self) -> BrokerResult<()> {
        let _queue = self.lock_queue()?;
        let mut state = BrokerState::read(&self.queue_dir);
        let port = state.port.ok_or_else(|| {
            BrokerFailure::new(
                FailureCode::ContainerFailed,
                "the broker's state names no port to restart it on",
            )
        })?;
        let container = self.spec(port)?;
        let restarted = broker::restart(
            &self.broker_ports(),
            &container,
            self.health_timeout,
            self.health_interval,
        );
        state.state = Some(match &restarted {
            Ok(_) => "running".to_owned(),
            Err(failure) => failure.code.as_str().to_owned(),
        });
        if restarted.is_ok() {
            state.started_at = Some(unix_now());
        }
        self.write_state(&state);
        restarted.map(|_| ())
    }

    fn stop(&self) -> BrokerResult<StopReport> {
        QueueBroker::stop(self)
    }

    fn running_port(&self) -> Result<u16, String> {
        let state = BrokerState::read(&self.queue_dir);
        let not_ready = "the queue's broker is not ready yet";
        let port = state
            .port
            .ok_or_else(|| format!("{not_ready}: its state names no port"))?;
        if state.state.as_deref() != Some("running") || state.build.as_deref() != Some(&self.build)
        {
            return Err(format!(
                "{not_ready}: its state is {} of build {}",
                state.state.as_deref().unwrap_or("unknown"),
                state.build.as_deref().unwrap_or("unknown")
            ));
        }
        // A broker recorded as running is looked at more than once: one
        // missed answer on a loaded host would leave the whole run without
        // the tools (task 1255).
        let mut missed = Vec::new();
        for probe in 0..RUNNING_PORT_PROBES {
            if probe > 0 {
                std::thread::sleep(self.health_interval);
            }
            match self.ports.health.probe(port) {
                Ok(health)
                    if health.status == "ok"
                        && health.protocol == PROTOCOL_VERSION
                        && health.build == self.build =>
                {
                    return Ok(port);
                }
                Ok(health) => {
                    return Err(format!(
                        "the broker on port {port} answered status {} in protocol {} of build {}",
                        health.status, health.protocol, health.build
                    ));
                }
                Err(error) => missed.push(error),
            }
        }
        Err(format!(
            "the broker recorded as running on port {port} did not answer its health {RUNNING_PORT_PROBES} times: {}",
            missed.join("; ")
        ))
    }
}

/// The real ports: `podman` (`None` is `podman` on `PATH`) that waits for a
/// lost connection to the machine ([`Reconnecting`], task 1162), the
/// host-wide lock under `lock_home` ([`super::broker_podman::machine_lock_home`]),
/// the health over loopback, and the image built from the material this
/// binary embeds (never a checkout's files).
pub fn system_ports(podman: Option<&Path>, lock_home: &Path) -> BrokerPorts {
    use super::broker_image::EmbeddedSource;
    use super::broker_podman::{HttpHealth, PodmanCli};
    let source: Arc<dyn ImageSource + Send + Sync> = Arc::new(EmbeddedSource::of_this_build());
    let podman: Arc<dyn Podman + Send + Sync> = match PodmanCli::resolve(podman) {
        Ok(podman) => Arc::new(Reconnecting::new(podman)),
        Err(failure) => Arc::new(MissingPodman(failure)),
    };
    BrokerPorts {
        podman,
        host_lock: Arc::new(FileLock::machine(lock_home)),
        health: Arc::new(HttpHealth::default()),
        source,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use dagq_broker_protocol::HealthResponse;

    /// A podman that answers every command with its script's first match
    /// and records them.
    #[derive(Default)]
    struct Fake {
        calls: Mutex<Vec<String>>,
        image: bool,
    }

    fn ok(stdout: &str) -> PodmanOutput {
        PodmanOutput {
            success: true,
            code: Some(0),
            stdout: stdout.to_owned(),
            stderr: String::new(),
        }
    }

    impl Podman for Fake {
        fn run(&self, args: &[String]) -> BrokerResult<PodmanOutput> {
            let line = args.join(" ");
            self.calls.lock().unwrap().push(line.clone());
            Ok(if line.starts_with("machine list") {
                ok(r#"[{"Name":"dagq","Running":true}]"#)
            } else if line.contains("image exists") {
                PodmanOutput {
                    success: self.image,
                    code: Some(if self.image { 0 } else { 1 }),
                    ..PodmanOutput::default()
                }
            } else if line.contains("container exists") {
                PodmanOutput {
                    success: false,
                    code: Some(1),
                    ..PodmanOutput::default()
                }
            } else {
                ok("")
            })
        }
    }

    struct Lock;

    impl HostLock for Lock {
        fn hold(&self) -> BrokerResult<Box<dyn std::any::Any>> {
            Ok(Box::new(()))
        }
    }

    struct Healthy;

    impl HealthProbe for Healthy {
        fn probe(&self, _port: u16) -> Result<HealthResponse, String> {
            Ok(HealthResponse {
                status: "ok".into(),
                build: crate::VERSION.into(),
                protocol: PROTOCOL_VERSION,
            })
        }
    }

    struct Source;

    impl ImageSource for Source {
        fn stage(&self, _dir: &Path) -> BrokerResult<String> {
            Ok("1.0".into())
        }
    }

    fn queue_broker(dir: &Path, podman: Arc<Fake>) -> QueueBroker {
        let mut broker = QueueBroker::new(
            dir.join("q"),
            dir.join("q/runs"),
            "hash".into(),
            Some(dir.join("repo/.git")),
            BrokerPorts {
                podman,
                host_lock: Arc::new(Lock),
                health: Arc::new(Healthy),
                source: Arc::new(Source),
            },
        );
        broker.port = Some(40000);
        broker.health_timeout = Duration::ZERO;
        broker
    }

    #[test]
    fn start_records_building_then_running_and_stop_records_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let podman = Arc::new(Fake::default());
        let broker = queue_broker(dir.path(), podman.clone());
        let report = broker.ensure().unwrap();
        assert!(report.image_built);
        assert!(report.build_ms.is_some());
        let state = BrokerState::read(&broker.queue_dir);
        assert_eq!(state.state.as_deref(), Some("running"));
        assert_eq!(state.port, Some(40000));
        assert_eq!(state.build.as_deref(), Some(crate::VERSION));
        assert!(state.started_at.is_some());
        assert!(broker.health().is_ok());
        // Recorded as running this build and answering it: its port.
        assert_eq!(broker.running_port(), Ok(40000));
        let other = QueueBroker {
            build: "0.0.0-other".into(),
            ..queue_broker(dir.path(), Arc::new(Fake::default()))
        };
        let refused = other.running_port().unwrap_err();
        assert!(refused.contains("not ready yet"), "{refused}");
        assert!(broker.restart().is_ok());
        // The restart made the container again.
        let runs = podman
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call.contains(" run --detach --name dagq-broker-hash "))
            .count();
        assert_eq!(runs, 2);
        broker.stop().unwrap();
        let state = BrokerState::read(&broker.queue_dir);
        assert_eq!(state.state.as_deref(), Some("stopped"));
        let stopped = broker.running_port().unwrap_err();
        assert!(stopped.contains("its state is stopped"), "{stopped}");

        // Without a repository there is no container to make.
        let unknown = QueueBroker {
            git_common_dir: None,
            ..queue_broker(dir.path(), Arc::new(Fake::default()))
        };
        assert_eq!(
            unknown.ensure().unwrap_err().code,
            FailureCode::RepositoryUnknown
        );
        // A missing podman is the state, not a panic.
        let missing = MissingPodman(BrokerFailure::new(FailureCode::PodmanMissing, "none"));
        assert_eq!(
            missing.run(&[]).unwrap_err().code,
            FailureCode::PodmanMissing
        );
    }

    /// A health that misses `misses` looks, then answers with `build`.
    struct Missing {
        misses: Mutex<usize>,
        build: String,
        looks: Mutex<usize>,
    }

    impl HealthProbe for Missing {
        fn probe(&self, _port: u16) -> Result<HealthResponse, String> {
            *self.looks.lock().unwrap() += 1;
            let mut misses = self.misses.lock().unwrap();
            if *misses > 0 {
                *misses -= 1;
                return Err("timed out".into());
            }
            Ok(HealthResponse {
                status: "ok".into(),
                build: self.build.clone(),
                protocol: PROTOCOL_VERSION,
            })
        }
    }

    #[test]
    fn a_running_broker_is_looked_at_again_before_the_claim_gives_up_on_it() {
        let dir = tempfile::tempdir().unwrap();
        queue_broker(dir.path(), Arc::new(Fake::default()))
            .ensure()
            .unwrap();
        let with = |misses: usize, build: &str| {
            let health = Arc::new(Missing {
                misses: Mutex::new(misses),
                build: build.into(),
                looks: Mutex::new(0),
            });
            let mut broker = queue_broker(dir.path(), Arc::new(Fake::default()));
            broker.health_interval = Duration::ZERO;
            broker.ports.health = health.clone();
            (broker.running_port(), *health.looks.lock().unwrap())
        };
        // Two missed looks on a loaded host: the third answers.
        assert_eq!(with(2, crate::VERSION), (Ok(40000), 3));
        // Never answering: why, with each miss.
        let (port, looks) = with(5, crate::VERSION);
        let why = port.unwrap_err();
        assert_eq!(looks, 3);
        assert!(
            why.contains("did not answer its health 3 times: timed out; timed out; timed out"),
            "{why}"
        );
        // An answer of another build is not looked at again.
        let (port, looks) = with(0, "0.0.0-other");
        let why = port.unwrap_err();
        assert_eq!(looks, 1);
        assert!(why.contains("of build 0.0.0-other"), "{why}");
    }
}
