//! The containers of a queue's runs in the dagq machine ([Container
//! runtime]): [`PodmanRunContainers`], the adapter of the
//! [`RunContainers`] port. Before a turn it reads where `dagq.toml` puts
//! the build output, has the machine's parts made ready through the
//! [`RunMachine`] port the host's context lends (the machine, the image,
//! the run's relay, its build output volume and the queue's worker cache
//! volumes of the directories `dagq.toml` lists), and reads `[run.env]`
//! and the containers' limits; it hands
//! the run token over as a 0600 file of the run directory; it starts
//! `podman --connection dagq run`; it removes a run's container by its
//! name. The limits are the `[container]` table of the host-wide
//! `host.toml` ([`load_container_limits`]).
//!
//! The verifications of `integrate` set to the container have
//! [`PodmanVerificationContainers`], the adapter of the
//! [`VerificationContainers`] port.
//!
//! [Container runtime]: ../../docs/design/container-runtime.md

use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};

use std::os::unix::fs::DirBuilderExt;

use super::adapters::GitRepository;
use super::agent_dir::Directory;
use super::podman_machine::PodmanCli;
use super::queue_service::{queue_dir_of, write_private};
use super::run_env::{
    load_build_output_dir, load_run_env, load_worker_cache_dirs, parse_positive, strip_comment,
};
use crate::application::CommandSpec;
use crate::application::podman_machine::RunMachine;
use crate::application::run_container::{
    PreparedRun, PreparedVerification, RunContainers, VerificationContainers,
};
use crate::domain::podman_machine::{MACHINE_NAME, MachineMounts, QueueKey};
use crate::domain::run_clone::CLONES_DIR;
use crate::domain::run_container::{
    ContainerLimits, FactsContainer, HOME_DIR, MATERIALS_DIR, STAGED_MATERIALS, TOKEN_FILE,
    VERIFICATION_TOKEN, facts_command, facts_run_args,
};
use crate::domain::{CommitSha, RunId, TaskRun};

/// How long the removal of a run's container may take: a podman whose
/// machine hangs does not hold a stop of the run.
pub const REMOVE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// How long a facts-only turn may take: it runs no agent, only git in the
/// clone, so one that does not end is a machine that hangs, not work.
pub const FACTS_TURN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

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
        let worker_cache = load_worker_cache_dirs(checkout)?;
        let image = self.machine.ready(
            run.id().as_str(),
            build_output.is_some().then_some(run.id().as_str()),
        )?;
        self.machine.create_worker_cache(&worker_cache)?;
        Ok(PreparedRun {
            queue: self.key()?,
            image,
            build_output,
            worker_cache,
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

    /// Only the machine and the image are made ready (no relay, no
    /// volume), the run token's copy a cut-off turn may have left in the
    /// run directory is removed first, and the container mounts only the
    /// clone and the run directory ([`facts_run_args`]).
    fn facts_turn(&self, run: &TaskRun) -> Result<()> {
        let image = self
            .machine
            .ready_image(run.id().as_str())
            .map_err(|error| anyhow::anyhow!("make the machine and the image ready: {error:#}"))?;
        let run_dir = Path::new(run.run_dir().context("missing run directory")?);
        let clone = Path::new(run.worktree_path().context("missing clone")?);
        let branch = run.branch().context("missing run branch")?;
        let token = run_dir.join(TOKEN_FILE);
        match fs::remove_file(&token) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("remove {}", token.display()));
            }
        }
        let command = facts_command(run_dir, clone, run.base_commit().as_str(), branch);
        let args = facts_run_args(&FactsContainer {
            queue: &self.key()?,
            run: run.id().as_str(),
            image: &image,
            clone,
            run_dir,
            limits: load_container_limits(self.host_wide.as_deref()).0,
            command: &command,
        })
        .map_err(|why| anyhow::anyhow!("the facts-only turn's container cannot be made: {why}"))?;
        let Some(output) = self.podman.inside_within(&args, FACTS_TURN_TIMEOUT)? else {
            // Its podman was killed; the container may still run in the
            // machine, and goes as a turn's does.
            let removed = self.remove(run.id());
            bail!(
                "the facts-only turn of run {} did not end within {FACTS_TURN_TIMEOUT:?}: it was stopped{}",
                run.id(),
                removed.err().map_or(String::new(), |error| format!(
                    ", and its container could not be removed: {error:#}"
                ))
            );
        };
        if !output.status.success() {
            bail!(
                "the facts-only turn of run {} exited with {}: {}",
                run.id(),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    }
}

/// The containers of the verifications of `integrate` of the queue at
/// `db` set to the container: the adapter of the
/// [`VerificationContainers`] port. A verification's directory is
/// `<queue dir>/verification/<verification>`, which the dagq machine
/// mounts (ADR-t2173-1 decision 1), its clone made by the repository the
/// queue is bound to with its git guarded.
#[derive(Clone)]
pub struct PodmanVerificationContainers {
    pub podman: PodmanCli,
    pub db: PathBuf,
    /// The main checkout the queue is bound to: where `dagq.toml` is read
    /// and the repository the clones are made from; `None` when it is
    /// bound to none, which runs no verification in the container.
    pub main_checkout: Option<PathBuf>,
    /// The host-wide `host.toml`: the containers' limits.
    pub host_wide: Option<PathBuf>,
    /// Makes the run's parts in the machine ready.
    pub machine: Arc<dyn RunMachine>,
}

impl PodmanVerificationContainers {
    fn key(&self) -> Result<QueueKey> {
        let queue_dir = queue_dir_of(&self.db);
        let name = queue_dir
            .file_name()
            .and_then(|name| name.to_str())
            .with_context(|| format!("{} names no queue", queue_dir.display()))?;
        QueueKey::new(name).map_err(anyhow::Error::msg)
    }

    /// Copy what is in the control side's directory `from` into the run
    /// directory's directory `into`, opened through descriptors: regular
    /// files are written to new inodes renamed into place (a symlink there
    /// is replaced, never followed), directories made and descended into
    /// without following a symlink; a symlink or any other entry in `from`
    /// is not copied.
    fn copy_into(from: &Path, into: &Directory) -> Result<()> {
        for entry in fs::read_dir(from).with_context(|| format!("read {}", from.display()))? {
            let entry = entry?;
            let (name, path) = (entry.file_name(), entry.path());
            let kind = entry.file_type()?;
            if kind.is_file() {
                let mut source =
                    fs::File::open(&path).with_context(|| format!("read {}", path.display()))?;
                into.replace(&name, |target| std::io::copy(&mut source, target).map(drop))
                    .with_context(|| format!("copy {}", path.display()))?;
            } else if kind.is_dir() {
                if into.kind(&name)?.is_some() {
                    into.remove_tree(&name)?;
                }
                into.mkdir(&name)?;
                Self::copy_into(&path, &into.child(&name)?)?;
            }
        }
        Ok(())
    }

    fn remove_named(&self, name: &str) -> Result<()> {
        let args = [
            "rm".to_owned(),
            "--force".to_owned(),
            "--ignore".to_owned(),
            "--time".to_owned(),
            "0".to_owned(),
            name.to_owned(),
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

impl VerificationContainers for PodmanVerificationContainers {
    fn prepare(
        &self,
        run: &TaskRun,
        commit: &CommitSha,
        token: &Path,
    ) -> Result<PreparedVerification> {
        let queue_dir = queue_dir_of(&self.db);
        let checkout = self.main_checkout.as_deref().context(
            "the queue is bound to no checkout, whose dagq.toml decides the verification's container",
        )?;
        let run_dir = Path::new(run.run_dir().context("missing run directory")?);
        let queue = self.key()?;
        // Unique: a directory there already is refused, never reused.
        let id = format!(
            "{}-{}",
            run.id(),
            &uuid::Uuid::new_v4().simple().to_string()[..VERIFICATION_SUFFIX]
        );
        let build_output = load_build_output_dir(checkout)?;
        let image = self.machine.ready(
            run.id().as_str(),
            build_output.is_some().then_some(id.as_str()),
        )?;
        let dir = MachineMounts::of_queue(&queue_dir).verifications.join(&id);
        let prepared = PreparedVerification {
            id,
            queue,
            image,
            staged: dir.join(STAGED_MATERIALS),
            materials: run_dir.join(MATERIALS_DIR),
            dir,
            build_output,
            limits: load_container_limits(self.host_wide.as_deref()).0,
        };
        // A directory refused (something there already) is not this
        // verification's, and is left as it is: only the volume goes.
        let cloned = GitRepository::inspect(checkout)
            .and_then(|repository| repository.create_verification_clone(&prepared.dir, commit));
        if let Err(error) = cloned {
            if prepared.build_output.is_some()
                && let Err(left) = self.machine.remove_build_output(&prepared.id)
            {
                tracing::warn!(error = %format_args!("{left:#}"), "the build output of the verification {} was not removed: {left:#}", prepared.id);
            }
            return Err(error);
        }
        let made = (|| -> Result<()> {
            let value = fs::read(token)
                .with_context(|| format!("read the verification's token {}", token.display()))?;
            write_private(&prepared.dir.join(VERIFICATION_TOKEN), &value)?;
            // The commands' home: the root filesystem is read-only.
            fs::DirBuilder::new()
                .mode(0o700)
                .create(prepared.dir.join(HOME_DIR))
                .context("create the verification's home")?;
            fs::DirBuilder::new()
                .mode(0o700)
                .create(&prepared.staged)
                .context("create the verification's materials")
        })();
        if let Err(error) = made {
            if let Err(left) = self.finish(&prepared) {
                tracing::warn!(error = %format_args!("{left:#}"), "the verification {} was not removed: {left:#}", prepared.id);
            }
            return Err(error);
        }
        Ok(prepared)
    }

    fn publish_materials(&self, verification: &PreparedVerification) -> Result<()> {
        let materials = &verification.materials;
        let (run_dir, name) = Directory::parent(materials)
            .with_context(|| format!("open the directory of {}", materials.display()))?;
        if run_dir.kind(name)?.is_some() {
            run_dir
                .remove_tree(name)
                .with_context(|| format!("remove {}", materials.display()))?;
        }
        run_dir
            .mkdir(name)
            .with_context(|| format!("create {}", materials.display()))?;
        let into = run_dir
            .child(name)
            .with_context(|| format!("open {}", materials.display()))?;
        Self::copy_into(&verification.staged, &into)
    }

    fn command_line(&self, args: &[String]) -> String {
        let mut line = vec![
            self.podman.program.to_string_lossy().into_owned(),
            "--connection".to_owned(),
            MACHINE_NAME.to_owned(),
        ];
        line.extend(args.iter().cloned());
        crate::application::naming::shell_join(&line)
    }

    fn remove_container(&self, verification: &PreparedVerification) -> Result<()> {
        let name = verification
            .queue
            .verification_container(&verification.id)
            .map_err(anyhow::Error::msg)?;
        self.remove_named(&name)
    }

    /// Every part is tried, and the first failure is returned: a container
    /// that cannot be removed leaves no directory or volume behind for it.
    fn finish(&self, verification: &PreparedVerification) -> Result<()> {
        let container = self.remove_container(verification);
        let volume = match verification.build_output {
            Some(_) => self.machine.remove_build_output(&verification.id),
            None => Ok(()),
        };
        let dir = GitRepository::remove_verification_dir(&verification.dir);
        container.and(volume).and(dir)
    }

    /// The directories named as [`Self::prepare`] names a verification of
    /// `run`; any other is left as it is. The build output volume is
    /// removed when it is there, whatever `dagq.toml` names now.
    fn remove_left(&self, run: &RunId) -> Result<Vec<String>> {
        let parent = MachineMounts::of_queue(&queue_dir_of(&self.db)).verifications;
        let entries = match fs::read_dir(&parent) {
            Ok(entries) => entries,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error).with_context(|| format!("read {}", parent.display())),
        };
        let queue = self.key()?;
        let prefix = format!("{run}-");
        let mut left = Vec::new();
        for entry in entries {
            let name = entry?.file_name();
            let Some(id) = name.to_str() else {
                continue;
            };
            let ours = id.strip_prefix(&prefix).is_some_and(|suffix| {
                suffix.len() == VERIFICATION_SUFFIX
                    && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
            });
            if ours {
                left.push(id.to_owned());
            }
        }
        if left.is_empty() {
            return Ok(left);
        }
        // The machine may have stopped since: podman reaches it only
        // running, as for a verification of its own.
        self.machine.ready(run.as_str(), None)?;
        let mut removed = Vec::new();
        for id in &left {
            let id = id.as_str();
            let container = queue
                .verification_container(id)
                .map_err(anyhow::Error::msg)?;
            self.remove_named(&container)?;
            self.machine.remove_build_output(id)?;
            GitRepository::remove_verification_dir(&parent.join(id))?;
            removed.push(id.to_owned());
        }
        Ok(removed)
    }
}

/// How many hex digits follow the run's ID in a verification's ID.
const VERIFICATION_SUFFIX: usize = 12;

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
            fn ready(&self, _: &str, _: Option<&str>) -> Result<String> {
                bail!("no machine in this test")
            }
            fn ready_image(&self, _: &str) -> Result<String> {
                bail!("no machine in this test")
            }
            fn remove_build_output(&self, _: &str) -> Result<()> {
                bail!("no machine in this test")
            }
            fn create_worker_cache(&self, _: &[String]) -> Result<()> {
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
