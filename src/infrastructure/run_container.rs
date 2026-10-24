//! The containers of a queue's runs in the dagq machine ([Container
//! runtime]): [`PodmanRunContainers`], the adapter of the
//! [`RunContainers`] port. Before a turn it reads where `dagq.toml` puts
//! the build output, has the machine's parts made ready through the
//! [`RunMachine`] port the host's context lends (the machine, the image,
//! the run's relay, its build output volume and the queue's worker cache
//! volumes of the directories `dagq.toml` lists), and reads `[run.env]`
//! and the containers' limits; it hands
//! the run token over as a 0600 file of the run directory; it starts
//! `podman --connection dagq run`; at the supervisor's call it starts the
//! sccache server and makes the guard in a running container by `podman
//! exec` and writes the gate the container's launcher waits for; it removes
//! a run's container by its name. The limits are the `[container]` table of the host-wide
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

use anyhow::{Context, Result, bail, ensure};

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
use crate::domain::sccache::{is_container_gate, is_container_request};
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

    /// The gates and the requests are removed through the run directory's
    /// descriptor: a link there goes, its target stays. The new turn's
    /// token is a fresh UUID's.
    fn clear_sccache(&self, run_dir: &Path) -> Result<String> {
        let dir =
            Directory::open(run_dir).with_context(|| format!("open {}", run_dir.display()))?;
        let names = names_of(
            &dir,
            |name| is_container_gate(name) || is_container_request(name),
            false,
        )
        .with_context(|| format!("read {}", run_dir.display()))?;
        remove_all(&dir, run_dir, &names)?;
        Ok(uuid::Uuid::new_v4().simple().to_string())
    }

    fn sccache_requests(&self, run_dir: &Path) -> Result<Vec<String>> {
        let dir =
            Directory::open(run_dir).with_context(|| format!("open {}", run_dir.display()))?;
        let mut names = names_of(&dir, is_container_request, true)
            .with_context(|| format!("read {}", run_dir.display()))?;
        names.sort();
        Ok(names)
    }

    fn take_sccache_requests(&self, run_dir: &Path, names: &[String]) -> Result<()> {
        let dir =
            Directory::open(run_dir).with_context(|| format!("open {}", run_dir.display()))?;
        remove_all(&dir, run_dir, names)
    }

    /// As `podman container inspect` says: one not there runs not. An
    /// inspect that fails is told from one of no container by
    /// `podman container exists` (exit 1 for none), and looked at again
    /// once when one is there by then (it started meanwhile); any other
    /// failure is an error, never a container that runs not.
    fn running_container(
        &self,
        run: &RunId,
    ) -> Result<Option<crate::application::run_container::RunningContainer>> {
        let name = self
            .key()?
            .run_container(run.as_str())
            .map_err(anyhow::Error::msg)?;
        let inspect = || -> Result<std::process::Output> {
            let args = [
                "container".to_owned(),
                "inspect".to_owned(),
                "--format".to_owned(),
                "{{.Id}} {{.State.Running}} {{json .Config.Env}}".to_owned(),
                name.clone(),
            ];
            self.podman
                .inside_within(&args, INSPECT_TIMEOUT)?
                .with_context(|| {
                    format!(
                        "podman container inspect {name} did not end within {INSPECT_TIMEOUT:?}"
                    )
                })
        };
        let mut output = inspect()?;
        if !output.status.success() {
            let exists = ["container".to_owned(), "exists".to_owned(), name.clone()];
            let looked = self
                .podman
                .inside_within(&exists, INSPECT_TIMEOUT)?
                .with_context(|| {
                    format!("podman container exists {name} did not end within {INSPECT_TIMEOUT:?}")
                })?;
            match looked.status.code() {
                Some(1) => return Ok(None),
                Some(0) => output = inspect()?,
                _ => {}
            }
            if !output.status.success() {
                bail!(
                    "podman container inspect {name} exited with {}: {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
        }
        Ok(
            parse_inspect(&String::from_utf8_lossy(&output.stdout)).map(|(id, gate, turn)| {
                crate::application::run_container::RunningContainer {
                    name,
                    id,
                    gate,
                    turn,
                }
            }),
        )
    }

    fn sccache_plan(&self, run: &TaskRun) -> Result<crate::domain::sccache::ContainerServer> {
        let checkout = self
            .main_checkout
            .as_deref()
            .context("the queue is bound to no checkout, whose dagq.toml decides the container")?;
        let run_dir = Path::new(run.run_dir().context("missing run directory")?);
        Ok(crate::domain::sccache::ContainerServer::of(
            &load_run_env(checkout, &queue_dir_of(&self.db), run_dir)?,
            &load_worker_cache_dirs(checkout)?,
        ))
    }

    fn start_sccache(
        &self,
        container: &str,
        program: &str,
        env: &[(String, String)],
    ) -> Result<()> {
        let command = [program.to_owned(), "--start-server".to_owned()];
        self.exec_in(container, env, &command, super::sccache::START_TIMEOUT)
    }

    fn make_sccache_guard(&self, container: &str, run_dir: &Path) -> Result<()> {
        let guard = crate::domain::sccache::container_guard(run_dir);
        let dir = guard.parent().context("the guard has no directory")?;
        let command = [
            "dagq".to_owned(),
            "relay".to_owned(),
            "guard".to_owned(),
            "--dir".to_owned(),
            dir.to_str()
                .context("the guard's path is not UTF-8")?
                .to_owned(),
        ];
        self.exec_in(container, &[], &command, GUARD_TIMEOUT)
    }

    fn claim_sccache_gate(
        &self,
        run_dir: &Path,
        name: &str,
        gate: &crate::domain::sccache::ContainerGate,
    ) -> crate::domain::sccache::GateClaim {
        use crate::domain::sccache::GateClaim;
        let dir = match Directory::open(run_dir) {
            Ok(dir) => dir,
            Err(error) => return GateClaim::Failed(format!("open {}: {error}", run_dir.display())),
        };
        // Written whole under a name of its own, then linked to the gate's:
        // no reader sees it half written, and the link fails where a gate
        // is.
        let tmp = format!(".{name}.{}.tmp", uuid::Uuid::new_v4().simple());
        if let Err(error) = dir.create_new(&tmp, gate.text().as_bytes()) {
            let _ = dir.remove(&tmp);
            return GateClaim::Failed(format!("{tmp}: {error}"));
        }
        let linked = dir.link(&tmp, name);
        let _ = dir.remove(&tmp);
        match linked {
            Ok(()) => GateClaim::Won,
            Err(error) if error.kind() == ErrorKind::AlreadyExists => match read_gate(&dir, name) {
                Ok(text) => GateClaim::Lost(text.unwrap_or_default()),
                Err(error) => GateClaim::Lost(format!("unreadable: {error:#}")),
            },
            Err(error) => GateClaim::Failed(format!("{name}: {error}")),
        }
    }

    fn read_sccache_gate(&self, run_dir: &Path, name: &str) -> Result<Option<String>> {
        let dir =
            Directory::open(run_dir).with_context(|| format!("open {}", run_dir.display()))?;
        read_gate(&dir, name)
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

/// How long one look at whether a run's container runs may take.
pub const INSPECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// How long the making of a turn container's guard may take: one link.
pub const GUARD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// What `podman container inspect --format '{{.Id}} {{.State.Running}}
/// {{json .Config.Env}}'` printed of a container: its ID, and the sccache
/// gate and the turn's token its environment names
/// ([`CONTAINER_GATE_VAR`](crate::domain::sccache::CONTAINER_GATE_VAR),
/// [`CONTAINER_TURN_VAR`](crate::domain::sccache::CONTAINER_TURN_VAR)),
/// when it runs; `None` when it does not, or the text is not of that form.
fn parse_inspect(text: &str) -> Option<(String, Option<String>, Option<String>)> {
    let mut parts = text.trim().splitn(3, ' ');
    let (id, running, env) = (parts.next()?, parts.next()?, parts.next()?);
    if running != "true" || id.is_empty() {
        return None;
    }
    let env: Vec<String> = serde_json::from_str(env).ok()?;
    let value = |var: &str| {
        let prefix = format!("{var}=");
        env.iter()
            .find_map(|pair| pair.strip_prefix(&prefix))
            .map(str::to_owned)
    };
    Some((
        id.to_owned(),
        value(crate::domain::sccache::CONTAINER_GATE_VAR),
        value(crate::domain::sccache::CONTAINER_TURN_VAR),
    ))
}

/// The names in `dir` that are `of` the turns' sccache files (their gates
/// or requests), with `files_only` only of regular files (a link is not
/// followed).
fn names_of(
    dir: &Directory,
    of: impl Fn(&str) -> bool,
    files_only: bool,
) -> std::io::Result<Vec<String>> {
    let mut names = Vec::new();
    for name in dir.names()? {
        let Some(name) = name.to_str().filter(|name| of(name)) else {
            continue;
        };
        if !files_only || dir.kind(name)? == Some(crate::application::EntryKind::File) {
            names.push(name.to_owned());
        }
    }
    Ok(names)
}

/// Remove `names` in `dir` (the run directory `run_dir`), any of them not
/// there already left so.
fn remove_all(dir: &Directory, run_dir: &Path, names: &[String]) -> Result<()> {
    for name in names {
        match dir.remove(name) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("remove {name} in {}", run_dir.display()));
            }
        }
    }
    Ok(())
}

/// The text of the sccache gate `name` in `dir`, `None` when there is
/// none: read through a descriptor opened without following a link and
/// without blocking (a FIFO there holds no one), only from a regular file,
/// and only its first bytes.
fn read_gate(dir: &Directory, name: &str) -> Result<Option<String>> {
    let file = match dir.read_file(name) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("read the sccache gate"),
    };
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut std::io::Read::take(file, 4096), &mut bytes)
        .context("read the sccache gate")?;
    Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
}

impl PodmanRunContainers {
    /// `podman exec` `command` with `env` in the running container of the
    /// ID `container`, given at most `limit` (it is stopped then). Its
    /// output goes to a file, never a pipe: a server it leaves running
    /// keeps what it inherited.
    fn exec_in(
        &self,
        container: &str,
        env: &[(String, String)],
        command: &[String],
        limit: std::time::Duration,
    ) -> Result<()> {
        // An ID is hex: nothing of it is an option or a path.
        ensure!(
            !container.is_empty() && container.bytes().all(|byte| byte.is_ascii_alphanumeric()),
            "{container:?} is no container's ID"
        );
        let name = container;
        let mut args = vec![
            "--connection".to_owned(),
            MACHINE_NAME.to_owned(),
            "exec".to_owned(),
        ];
        for (key, value) in env {
            args.extend(["--env".to_owned(), format!("{key}={value}")]);
        }
        args.push(name.to_owned());
        args.extend(command.iter().cloned());
        let program = command.first().map_or("", String::as_str);
        // In the queue's directory, which no container mounts, and gone
        // from it once open.
        let log_path = queue_dir_of(&self.db).join(format!("container-exec-{container}.log"));
        let log = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&log_path)
            .with_context(|| format!("create {}", log_path.display()))?;
        let _ = fs::remove_file(&log_path);
        let mut child = std::process::Command::new(&self.podman.program)
            .args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log.try_clone()?)
            .spawn()
            .with_context(|| format!("run {}", self.podman.program.display()))?;
        let started = std::time::Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if started.elapsed() >= limit {
                let _ = child.kill();
                let _ = child.wait();
                bail!("{program} in {name} did not end within {limit:?}");
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        if !status.success() {
            let mut output = String::new();
            let mut log = log;
            let _ = std::io::Seek::rewind(&mut log);
            let _ = std::io::Read::read_to_string(
                &mut std::io::Read::take(&mut log, 4096),
                &mut output,
            );
            bail!(
                "{program} in {name} exited with {status}: {}",
                output.trim()
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

    /// The inspect of a running container gives its ID and the gate and
    /// the turn's token its wrapper named; one that does not run, or text
    /// of another form, gives none.
    #[test]
    fn an_inspect_gives_a_running_container_s_id_and_its_gate() {
        assert_eq!(
            parse_inspect(
                "abc123 true [\"A=1\",\"DAGQ_SCCACHE_GATE=/q/runs/r1/sccache-gate.t1\",\"DAGQ_SCCACHE_TURN=t1\"]\n"
            ),
            Some((
                "abc123".to_owned(),
                Some("/q/runs/r1/sccache-gate.t1".to_owned()),
                Some("t1".to_owned())
            ))
        );
        assert_eq!(parse_inspect("abc123 true null"), None);
        assert_eq!(
            parse_inspect("abc123 true []"),
            Some(("abc123".to_owned(), None, None))
        );
        for text in ["abc123 false []", "abc123 true", "", "abc123 true [1]"] {
            assert_eq!(parse_inspect(text), None, "{text}");
        }
    }

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
