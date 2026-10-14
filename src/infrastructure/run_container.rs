//! The containers of a queue's runs in the dagq machine ([Container
//! runtime]): [`PodmanRunContainers`], the adapter of the
//! [`RunContainers`] port. Before a turn it reads where `dagq.toml` puts
//! the build output, has the machine's parts made ready through the
//! [`RunMachine`] port the host's context lends (the machine, the image,
//! the run's relay and its build output volume), and reads `[run.env]`
//! and the containers' limits; it hands
//! the run token over as a 0600 file of the run directory; it starts
//! `podman --connection dagq run`; it removes a run's container by its
//! name. The limits are the `[container]` table of the host-wide
//! `host.toml` ([`load_container_limits`]).
//!
//! [Container runtime]: ../../docs/design/container-runtime.md

use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};

use super::podman_machine::PodmanCli;
use super::queue_service::{queue_dir_of, write_private};
use super::run_env::{load_build_output_dir, load_run_env, parse_positive, strip_comment};
use crate::application::CommandSpec;
use crate::application::podman_machine::RunMachine;
use crate::application::run_container::{PreparedRun, RunContainers};
use crate::domain::podman_machine::{MACHINE_NAME, QueueKey};
use crate::domain::run_clone::CLONES_DIR;
use crate::domain::run_container::{ContainerLimits, HOME_DIR, TOKEN_FILE};
use crate::domain::{RunId, TaskRun};

/// How long the removal of a run's container may take: a podman whose
/// machine hangs does not hold a stop of the run.
pub const REMOVE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// The containers of the runs of the queue at `db`, in the dagq machine.
#[derive(Clone)]
pub struct PodmanRunContainers {
    pub podman: PodmanCli,
    pub db: PathBuf,
    /// The main checkout the queue is bound to: where `dagq.toml`, the
    /// image's files and `[run.env]` are read; `None` when it is bound to
    /// none, which runs no turn in the container.
    pub main_checkout: Option<PathBuf>,
    /// The host-wide `host.toml`: the containers' limits.
    pub host_wide: Option<PathBuf>,
    /// Makes the run's parts in the machine ready.
    pub machine: Arc<dyn RunMachine>,
}

impl PodmanRunContainers {
    /// The containers of the queue at `db`, bound to `main_checkout`,
    /// whose parts `machine` makes ready.
    pub fn of_queue(
        podman: PodmanCli,
        db: &Path,
        main_checkout: Option<PathBuf>,
        host_wide: Option<PathBuf>,
        machine: Arc<dyn RunMachine>,
    ) -> Self {
        Self {
            podman,
            db: db.to_path_buf(),
            main_checkout,
            host_wide,
            machine,
        }
    }

    fn key(&self) -> Result<QueueKey> {
        let queue_dir = queue_dir_of(&self.db);
        let name = queue_dir
            .file_name()
            .and_then(|name| name.to_str())
            .with_context(|| format!("{} names no queue", queue_dir.display()))?;
        QueueKey::new(name).map_err(anyhow::Error::msg)
    }
}

impl RunContainers for PodmanRunContainers {
    fn prepare(&self, run: &TaskRun) -> Result<PreparedRun> {
        let queue_dir = queue_dir_of(&self.db);
        let checkout = self
            .main_checkout
            .as_deref()
            .context("the queue is bound to no checkout, whose dagq.toml decides the container")?;
        let run_dir = Path::new(run.run_dir().context("missing run directory")?);
        let build_output = load_build_output_dir(checkout)?;
        let image = self
            .machine
            .ready(run.id().as_str(), build_output.is_some())?;
        Ok(PreparedRun {
            queue: self.key()?,
            image,
            build_output,
            run_env: load_run_env(checkout, &queue_dir, run_dir)?,
            limits: load_container_limits(self.host_wide.as_deref()).0,
        })
    }

    fn prepare_run_dir(&self, run_dir: &Path, credential: &Path) -> Result<PathBuf> {
        let token = fs::read(credential)
            .with_context(|| format!("read the worker's token {}", credential.display()))?;
        // Renamed into place: a symlink a worker left there is replaced,
        // never followed.
        let file = run_dir.join(TOKEN_FILE);
        write_private(&file, &token)?;
        let home = run_dir.join(HOME_DIR);
        match fs::symlink_metadata(&home) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => bail!("{} is there and is no directory", home.display()),
            Err(error) if error.kind() == ErrorKind::NotFound => {
                fs::create_dir(&home).with_context(|| format!("create {}", home.display()))?;
                fs::set_permissions(&home, fs::Permissions::from_mode(0o700))
                    .with_context(|| format!("narrow {}", home.display()))?;
            }
            Err(error) => {
                return Err(error).with_context(|| format!("look at {}", home.display()));
            }
        }
        Ok(file)
    }

    fn command(&self, args: &[String]) -> CommandSpec {
        let mut command = CommandSpec::new(&self.podman.program);
        command.args(["--connection", MACHINE_NAME]).args(args);
        command
    }

    /// Only a run that ever had a container (its clone, or its relay, is
    /// there) is asked of podman: a run on the host has none, and its
    /// stops need no podman on the host. The run token's copy in the run
    /// directory goes with the container.
    fn remove(&self, run: &RunId) -> Result<()> {
        let queue_dir = queue_dir_of(&self.db);
        let clone = queue_dir.join(CLONES_DIR).join(run.as_str());
        let relay = queue_dir
            .join(super::broker_relay::RELAY_DIR)
            .join(format!("{}.json", run.as_str()));
        if !clone.exists() && !relay.exists() {
            return Ok(());
        }
        // The token's copy lives no longer than a turn's container.
        let token = queue_dir.join("runs").join(run.as_str()).join(TOKEN_FILE);
        match fs::remove_file(&token) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("remove {}", token.display()));
            }
        }
        let name = self
            .key()?
            .run_container(run.as_str())
            .map_err(anyhow::Error::msg)?;
        let args = [
            "rm".to_owned(),
            "--force".to_owned(),
            "--ignore".to_owned(),
            "--time".to_owned(),
            "0".to_owned(),
            name.clone(),
        ];
        let output = self
            .podman
            .inside_within(&args, REMOVE_TIMEOUT)?
            .with_context(|| {
                format!("podman rm {name} did not end within {REMOVE_TIMEOUT:?}: it was stopped")
            })?;
        if !output.status.success() {
            bail!(
                "podman rm {name} exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    }
}

/// The `[container]` table of a `host.toml`'s text, `label` naming the
/// file in the warnings: the containers' limits ([`ContainerLimits`]);
/// `None` when it has none. A value of the wrong shape keeps its default
/// and is a warning.
pub fn parse_host_container(text: &str, label: &str) -> Option<(ContainerLimits, Vec<String>)> {
    let mut found: Option<(ContainerLimits, Vec<String>)> = None;
    let mut inside = false;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(header) = line.strip_prefix('[') {
            let name = strip_comment(header).strip_suffix(']').map(str::trim);
            inside = name == Some("container") && found.is_none();
            if inside {
                found = Some((ContainerLimits::default(), Vec::new()));
            }
            continue;
        }
        let Some((limits, warnings)) = found.as_mut().filter(|_| inside) else {
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
            ("memory_mib", Ok(n)) => limits.memory_mib = n.unsigned_abs(),
            ("cpus" | "pids", Ok(n)) => match u32::try_from(n) {
                Ok(n) if key == "cpus" => limits.cpus = n,
                Ok(n) => limits.pids = n,
                Err(_) => warnings.push(format!("{at}: {key}: {n} is too large")),
            },
            ("memory_mib" | "cpus" | "pids", Err(error)) => {
                warnings.push(format!("{at}: {key}: {error:#}; its default holds"));
            }
            _ => warnings.push(format!(
                "{at}: unknown key {key} in [container]; the keys are memory_mib, cpus, pids"
            )),
        }
    }
    found
}

/// The containers' limits: the `[container]` of the host-wide `host.toml`
/// (`host_wide`), else the defaults, with what was wrong in it. Only the
/// host-wide file counts, as for the machine the containers share.
pub fn load_container_limits(host_wide: Option<&Path>) -> (ContainerLimits, Vec<String>) {
    let Some(path) = host_wide else {
        return (ContainerLimits::default(), Vec::new());
    };
    match fs::read_to_string(path) {
        Ok(text) => parse_host_container(&text, &path.display().to_string())
            .unwrap_or_else(|| (ContainerLimits::default(), Vec::new())),
        Err(error) if error.kind() == ErrorKind::NotFound => {
            (ContainerLimits::default(), Vec::new())
        }
        Err(error) => (
            ContainerLimits::default(),
            vec![format!("read {}: {error}", path.display())],
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_host_s_container_table_sets_the_limits_and_keeps_a_default_for_a_bad_value() {
        let (limits, warnings) = parse_host_container(
            "[machine]\ncpus = 8\n[container]\nmemory_mib = 6144\ncpus = 4 # four\npids = 0\nsize = 1\n",
            "host.toml",
        )
        .unwrap();
        assert_eq!(
            limits,
            ContainerLimits {
                memory_mib: 6144,
                cpus: 4,
                pids: ContainerLimits::default().pids,
            }
        );
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings[1].contains("unknown key size"), "{warnings:?}");
        assert_eq!(parse_host_container("[machine]\ncpus = 8\n", "h"), None);
        assert_eq!(load_container_limits(None).0, ContainerLimits::default());
    }

    #[test]
    fn the_token_is_handed_over_as_a_private_file_that_replaces_a_planted_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let credential = dir.path().join("credential");
        fs::write(&credential, "tok").unwrap();
        let run_dir = dir.path().join("run");
        fs::create_dir(&run_dir).unwrap();
        let elsewhere = dir.path().join("elsewhere");
        fs::write(&elsewhere, "keep").unwrap();
        std::os::unix::fs::symlink(&elsewhere, run_dir.join(TOKEN_FILE)).unwrap();
        struct NoMachine;
        impl RunMachine for NoMachine {
            fn ready(&self, _: &str, _: bool) -> Result<String> {
                bail!("no machine in this test")
            }
        }
        let containers = PodmanRunContainers::of_queue(
            PodmanCli::default(),
            &dir.path().join("q").join("queue.db"),
            None,
            None,
            Arc::new(NoMachine),
        );
        let file = containers.prepare_run_dir(&run_dir, &credential).unwrap();
        assert_eq!(file, run_dir.join(TOKEN_FILE));
        assert_eq!(fs::read_to_string(&file).unwrap(), "tok");
        let metadata = fs::symlink_metadata(&file).unwrap();
        assert!(metadata.is_file());
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        assert_eq!(fs::read_to_string(&elsewhere).unwrap(), "keep");
        let home = fs::symlink_metadata(run_dir.join(HOME_DIR)).unwrap();
        assert!(home.is_dir());
        assert_eq!(home.permissions().mode() & 0o777, 0o700);
        // Again: the home is kept, the token rewritten.
        containers.prepare_run_dir(&run_dir, &credential).unwrap();
    }
}
