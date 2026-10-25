//! The container a worker's turn runs in ([Container runtime], ADR-t2113-3
//! decision 1, ADR-t2113-1 decision 6, ADR-t2114-2 decision 1, ADR-t2114-3
//! decision 1): one container of the dagq machine per turn of a run whose
//! worker is set to the container, named after the run so that whatever
//! stops the run removes it ([`QueueKey::run_container`]).
//!
//! What is decided here: the arguments of `podman run` ([`worker_run_args`]:
//! a non-root user, a read-only root filesystem, no capability, no
//! privilege escalation, the limits of [`ContainerLimits`], no network, and
//! as mounts only the run's clone, its directory, its socket, its build
//! output and its queue's worker cache), the command the container runs ([`launcher_command`], the
//! `dagq relay turn` of the image's client), and the environment that
//! command gives the agent from the run token's file ([`turn_env`]). The
//! token's value is never in the arguments: the container reads it from
//! [`TOKEN_FILE`] in the run directory.
//!
//!
//! The verification of `integrate` set to the container runs each of its
//! commands in a container of its own, hardened alike
//! ([`verification_run_args`]): it mounts the verification's directory
//! (its clone and its token's copy), the materials read-only, the run's
//! socket and its own build output, never a worker cache, and runs
//! [`verification_command`].
//!
//! [Container runtime]: ../../docs/design/container-runtime.md

use std::path::{Path, PathBuf};

use super::broker_relay::{CONTAINER_UID, FORWARD_PORT, container_env, run_socket_mount};
use super::podman_machine::QueueKey;
use super::queue_broker::RUN_HEADER;

/// The run token's file in the run directory: a copy of the worker's token
/// the control side writes (mode 0600, the run's) before each turn, which
/// the container's `dagq relay turn` reads into the agent's environment and
/// its `dagq` reads as its credential. It is the run's own token, which the
/// broker checks; no other secret is in the run directory.
pub const TOKEN_FILE: &str = "run-token";

/// The home of the agent in the container, in the run directory: the root
/// filesystem is read-only, and the session the next turn resumes must
/// outlive the turn's container.
pub const HOME_DIR: &str = "container-home";

/// Claude Code's configuration directory under [`HOME_DIR`]: the agent in
/// the container has `HOME` and no `CLAUDE_CONFIG_DIR` (the executor sets
/// neither from `[run.env]`), so its sessions' transcripts are under
/// `<run dir>/container-home/.claude/projects`.
pub const CLAUDE_DIR: &str = ".claude";

/// Where the transcripts of a container run's sessions are, as the control
/// side reads them ([`container_transcripts`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerTranscripts {
    /// The agent's home in the run directory ([`HOME_DIR`]): the container
    /// writes it, so it and everything under it are read without
    /// following a link (ADR-t2179-1 decision 9).
    pub home: PathBuf,
    /// The directory the container's turns run in, which names the
    /// session's project directory: the clone, mounted at its own path
    /// and the container's `--workdir` ([`worker_run_args`]).
    pub cwd: PathBuf,
}

/// Where the transcripts of the sessions a run's turns started in
/// `workplace` (the run's stored worktree path) are: for a run in a
/// clone, under the agent's home in its run directory and named after the
/// clone, the container's working directory; `None` for a run on the
/// host, whose transcripts are the host's Claude Code's
/// (`$CLAUDE_CONFIG_DIR`, else `~/.claude`). The reader looks at the
/// host's first, where a container run's review is.
pub fn container_transcripts(workplace: &Path) -> Option<ContainerTranscripts> {
    let run_dir = super::run_clone::run_dir_of(workplace)?;
    Some(ContainerTranscripts {
        home: run_dir.join(HOME_DIR),
        cwd: workplace.to_path_buf(),
    })
}

/// The agent's executable on the image's `PATH`.
pub const AGENT: &str = "claude";

/// The label of a run's container, valued with its run's ID (beside
/// [`super::broker_relay::QUEUE_LABEL`]).
pub const RUN_LABEL: &str = "dagq.run";

/// Take the sccache wrapper out of `env`, the `[run.env]` of a
/// verification in the container: until a protected cache exists, it
/// compiles without one (ADR-t2114-4 decision 4). Only a `RUSTC_WRAPPER`
/// that names sccache is taken out; any other wrapper, and an environment
/// without one, are left as they are (ADR-t2257-1 decision 5). The
/// sccache it named is returned when it was taken out.
pub fn without_sccache_wrapper(
    env: &mut Vec<(String, String)>,
) -> Option<super::sccache::SccacheTarget> {
    let named = super::sccache::SccacheTarget::of_pairs(env)?;
    env.retain(|(key, _)| key != super::sccache::WRAPPER_VAR);
    Some(named)
}

/// The exit code `podman run` gives when podman itself failed: the
/// container was not made or not started, so its command did not run.
pub const PODMAN_ERROR_EXIT: i32 = 125;

/// Whether `podman run`'s exit `code` says podman itself failed
/// ([`PODMAN_ERROR_EXIT`]) rather than the container's command: a command
/// that ran exits with its own code, and one cut off by a signal has none.
pub const fn podman_failed(code: Option<i32>) -> bool {
    matches!(code, Some(PODMAN_ERROR_EXIT))
}

/// The limits of each run's container (ADR-t2257-1 decision 6), from the
/// `[container]` table of the host-wide `host.toml` (`memory_mib`, `cpus`,
/// `pids`); a key left out keeps its default. The memory limit is also
/// what the container reserves of the machine before it starts
/// ([`super::machine_memory`], ADR-t3163-1). The defaults are modest so
/// that they hold for any project on a machine of the default size
/// ([`super::podman_machine::DEFAULT_MACHINE_MEMORY_MIB`] and
/// [`super::podman_machine::DEFAULT_MACHINE_CPUS`]): 2 GiB, so that a
/// worker's container and a verification's fit at once in what the
/// machine leaves the containers after its base and its relays, its CPUs,
/// and enough processes for a build's parallel jobs and a test suite's
/// threads. A project whose builds need more says so in the host's
/// `host.toml` (for this repository, the value its docs recommend), with
/// a machine made large enough.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContainerLimits {
    pub memory_mib: u64,
    pub cpus: u32,
    pub pids: u32,
}

pub const DEFAULT_CONTAINER_MEMORY_MIB: u64 = 2048;
pub const DEFAULT_CONTAINER_CPUS: u32 = 2;
pub const DEFAULT_CONTAINER_PIDS: u32 = 2048;

impl Default for ContainerLimits {
    fn default() -> Self {
        Self {
            memory_mib: DEFAULT_CONTAINER_MEMORY_MIB,
            cpus: DEFAULT_CONTAINER_CPUS,
            pids: DEFAULT_CONTAINER_PIDS,
        }
    }
}

/// Where a run's turn starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnPlace {
    /// A process of the host, through the host's executor, which refuses
    /// it in turn while `dagq.toml` sets the worker to the container.
    Host,
    /// The run's container ([`worker_run_args`]).
    Container,
}

/// Where the turn of a run starts, from whether the run works in a clone
/// (`in_clone`) and where `dagq.toml` puts the worker now (`worker`, `None`
/// when the wrapper reads no runtime): a run in a clone runs its turns in
/// its container only, and never on the host, whose process would run the
/// worker's code there (fail closed); a run on the host is the host
/// executor's to start or refuse. `Err` says why the turn is refused.
pub fn turn_place(
    in_clone: bool,
    worker: Option<super::container_runtime::Runtime>,
) -> Result<TurnPlace, String> {
    use super::container_runtime::Runtime;
    match (in_clone, worker) {
        (false, _) => Ok(TurnPlace::Host),
        (true, Some(Runtime::Container)) => Ok(TurnPlace::Container),
        (true, Some(Runtime::Host)) => Err(
            "it works in a clone for the container, and dagq.toml sets the worker to the host now; dagq refuses to start its turn on the host".to_owned(),
        ),
        (true, None) => Err(
            "it works in a clone, and this wrapper reads no runtime; dagq refuses to start its turn on the host".to_owned(),
        ),
    }
}

/// One turn's container of a run.
#[derive(Debug, Clone)]
pub struct WorkerContainer<'a> {
    pub queue: &'a QueueKey,
    pub run: &'a str,
    /// The image the turn runs ([`super::container_image`]).
    pub image: &'a str,
    /// The run's clone, which the worker writes, mounted at its own path.
    pub clone: &'a Path,
    /// The run's directory (its prompt, requests, receipt, the bundles,
    /// the state and the token's file), mounted at its own path.
    pub run_dir: &'a Path,
    /// The directory in the container the run's build output volume is
    /// mounted at (`[container.build_output]` of `dagq.toml`); `None`
    /// mounts no such volume.
    pub build_output: Option<&'a str>,
    /// The directories in the container the queue's worker cache is
    /// mounted at (`[container.worker_cache]` of `dagq.toml`), each from
    /// its volume of [`QueueKey::worker_cache_volume_of`]; empty mounts
    /// none.
    pub worker_cache: &'a [String],
    pub limits: ContainerLimits,
    /// The variables of the agent: none holds a secret.
    pub env: &'a [(String, String)],
    /// The variables the container is started without whatever its image
    /// sets (`--unsetenv`): `RUSTC_WRAPPER` of a turn whose sccache the
    /// supervisor handles, which only the launcher gives the agent.
    pub unset_env: &'a [&'a str],
    /// What the container runs: [`launcher_command`].
    pub command: &'a [String],
}

/// The arguments after `podman --connection dagq` that run a turn of the
/// run in its container, removed when it ends (`--rm`) and named so that a
/// stop removes it. The user is the image's non-root one, mapped to the
/// machine's user that owns the mounted directories (`keep-id`), so the
/// worker writes its clone and nothing else of the machine; the root
/// filesystem is read-only, with a private `/tmp` the tests may run
/// scripts from; every capability is dropped and no privilege can be
/// gained; memory, CPUs and processes are limited; there is no network
/// (the forwarder reaches the broker through the run's socket). The mounts
/// are the clone, the run directory, the run's socket, the build output
/// and the queue's worker cache: no Podman or Docker socket, no `$HOME` of
/// the host, no cache of the host's, no landing worktree and no
/// verification's clone. Every path is checked to be absolute and free of
/// `,` and `:` so no mount option can be smuggled in; a worker cache's
/// directory that is not is refused as an error of `dagq.toml`.
pub fn worker_run_args(container: &WorkerContainer<'_>) -> Result<Vec<String>, String> {
    let name = container.queue.run_container(container.run)?;
    let clone = mount_path(container.clone)?;
    let run_dir = mount_path(container.run_dir)?;
    let mut args = hardened(container.queue, &name, container.run, container.limits);
    args.extend([
        "--volume".to_owned(),
        format!("{clone}:{clone}"),
        "--volume".to_owned(),
        format!("{run_dir}:{run_dir}"),
        "--volume".to_owned(),
        run_socket_mount(container.queue, container.run)?,
    ]);
    if let Some(dir) = container.build_output {
        let dir = mount_path(Path::new(dir))?;
        let volume = container.queue.build_output_volume(container.run)?;
        args.extend(["--volume".to_owned(), format!("{volume}:{dir}")]);
    }
    for dir in container.worker_cache {
        let dir = mount_path(Path::new(dir))
            .map_err(|why| format!("[container.worker_cache] of dagq.toml: {why}"))?;
        let volume = container.queue.worker_cache_volume_of(&dir);
        args.extend(["--volume".to_owned(), format!("{volume}:{dir}")]);
    }
    // The turn's prompt is podman's standard input, which only
    // `--interactive` hands the container's launcher and so the agent.
    args.extend(["--interactive".to_owned(), "--workdir".to_owned(), clone]);
    for key in container.unset_env {
        if key.is_empty() || key.contains('=') {
            return Err(format!("{key:?} cannot name a variable of the container"));
        }
        args.extend(["--unsetenv".to_owned(), (*key).to_owned()]);
    }
    finish_args(&mut args, container.env, container.image, container.command)?;
    Ok(args)
}

/// The container of a facts-only turn of a run that stopped
/// ([`facts_command`]).
#[derive(Debug, Clone)]
pub struct FactsContainer<'a> {
    pub queue: &'a QueueKey,
    pub run: &'a str,
    pub image: &'a str,
    pub clone: &'a Path,
    pub run_dir: &'a Path,
    pub limits: ContainerLimits,
    /// What the container runs: [`facts_command`].
    pub command: &'a [String],
}

/// The arguments after `podman --connection dagq` that run a facts-only
/// turn of the run: hardened and named as a turn's container (so that it
/// cannot start while one of the run's is there, and a stop of the run
/// removes it), with only the clone and the run directory mounted, as the
/// executor reads the one and writes its facts to the other. No socket of
/// the run's relay to the queue broker, no build output, no worker cache
/// and no variable: what is left of the run cannot reach anything outside
/// the two after its run stopped.
pub fn facts_run_args(container: &FactsContainer<'_>) -> Result<Vec<String>, String> {
    let name = container.queue.run_container(container.run)?;
    let clone = mount_path(container.clone)?;
    let run_dir = mount_path(container.run_dir)?;
    let mut args = hardened(container.queue, &name, container.run, container.limits);
    args.extend([
        "--volume".to_owned(),
        format!("{clone}:{clone}"),
        "--volume".to_owned(),
        format!("{run_dir}:{run_dir}"),
        "--workdir".to_owned(),
        clone,
    ]);
    finish_args(&mut args, &[], container.image, container.command)?;
    Ok(args)
}

/// The arguments every container of a run starts with, named `name`,
/// removed when it ends: the non-root user mapped to the machine's user
/// that owns the mounts, the read-only root filesystem with a private
/// `/tmp`, no capability, no privilege escalation, `limits`, no network.
fn hardened(queue: &QueueKey, name: &str, run: &str, limits: ContainerLimits) -> Vec<String> {
    let user = format!("{CONTAINER_UID}:{CONTAINER_UID}");
    [
        "run",
        "--rm",
        "--name",
        name,
        "--label",
        &format!("{}={}", super::broker_relay::QUEUE_LABEL, queue.as_str()),
        "--label",
        &format!("{RUN_LABEL}={run}"),
        "--userns",
        &format!("keep-id:uid={CONTAINER_UID},gid={CONTAINER_UID}"),
        "--user",
        &user,
        "--read-only",
        "--tmpfs",
        "/tmp:rw,exec,nosuid,nodev",
        "--cap-drop",
        "all",
        "--security-opt",
        "no-new-privileges",
        "--memory",
        &format!("{}m", limits.memory_mib),
        "--cpus",
        &limits.cpus.to_string(),
        "--pids-limit",
        &limits.pids.to_string(),
        "--network",
        "none",
    ]
    .iter()
    .map(|arg| (*arg).to_owned())
    .collect()
}

/// Put `env`, the image and the command after the mounts of `args`.
fn finish_args(
    args: &mut Vec<String>,
    env: &[(String, String)],
    image: &str,
    command: &[String],
) -> Result<(), String> {
    for (key, value) in env {
        if key.is_empty() || key.contains('=') {
            return Err(format!("{key:?} cannot name a variable of the container"));
        }
        args.extend(["--env".to_owned(), format!("{key}={value}")]);
    }
    args.push(image.to_owned());
    args.extend(command.iter().cloned());
    Ok(())
}

/// The directory of the run directory a verification's materials are in
/// (ADR-t2257-1 decision 4 (b)): what the preparation command `dagq.toml`
/// declares made on the host before the verification, and what the
/// runtime hands the verification's commands (the known failures of the
/// landing's command). They are first written to the verification's own
/// [`STAGED_MATERIALS`], which the worker never reaches, and then copied
/// here anew for each verification in the container through the run
/// directory's descriptors, following no symlink the worker put there; the
/// container mounts this copy read-only.
pub const MATERIALS_DIR: &str = "verification-materials";

/// The directory in the verification's directory the preparation command
/// writes the materials to: the control side's alone until the
/// verification's container starts.
pub const STAGED_MATERIALS: &str = "materials";

/// The variable that names the materials' directory: the staged one
/// ([`STAGED_MATERIALS`]) for the preparation command on the host, the run
/// directory's copy ([`MATERIALS_DIR`]) for each verification command in
/// the container.
pub const MATERIALS_ENV: &str = "DAGQ_VERIFICATION_MATERIALS";

/// The clone a verification's commands run in, in the verification's
/// directory: made by the control side from the commit that lands, with
/// its git guarded, and given no git of the control side once the
/// container started.
pub const VERIFICATION_CLONE: &str = "clone";

/// The copy of the verification's token in the verification's directory
/// (mode 0600): what `dagq relay verify` reads into the proxy variables.
/// The token reaches through the queue broker only the registries
/// `dagq.toml` names.
pub const VERIFICATION_TOKEN: &str = "token";

/// One verification of `integrate` in its container (ADR-t2113-3 decision
/// 1, ADR-t2114-4 decision 4, ADR-t2173-1 decisions 1 and 4).
#[derive(Debug, Clone)]
pub struct VerificationContainer<'a> {
    pub queue: &'a QueueKey,
    /// The run whose commit is verified: its socket is mounted.
    pub run: &'a str,
    /// The verification's ID: its container's and its build output's name.
    pub verification: &'a str,
    pub image: &'a str,
    /// The verification's directory (its clone and its token's copy),
    /// mounted at its own path.
    pub dir: &'a Path,
    /// The materials ([`MATERIALS_DIR`]), mounted read-only at its own path.
    pub materials: &'a Path,
    /// The directory in the container the verification's build output
    /// volume is mounted at (`[container.build_output]`); `None` mounts
    /// none.
    pub build_output: Option<&'a str>,
    pub limits: ContainerLimits,
    /// The variables of the commands: none holds a secret.
    pub env: &'a [(String, String)],
    /// What the container runs: [`verification_command`].
    pub command: &'a [String],
}

/// The arguments after `podman --connection dagq` that run one command of
/// a verification in its container, hardened as a worker's turn is
/// ([`worker_run_args`]). The mounts are the verification's directory, the
/// materials read-only, the run's socket and the verification's own build
/// output: no worker cache, no cache of the host's, no run directory, no
/// worker's clone and no landing worktree. The commands run in the clone
/// of the verification's directory.
pub fn verification_run_args(container: &VerificationContainer<'_>) -> Result<Vec<String>, String> {
    let name = container
        .queue
        .verification_container(container.verification)?;
    let dir = mount_path(container.dir)?;
    let materials = mount_path(container.materials)?;
    let mut args = hardened(container.queue, &name, container.run, container.limits);
    args.extend([
        "--volume".to_owned(),
        format!("{dir}:{dir}"),
        "--volume".to_owned(),
        format!("{materials}:{materials}:ro"),
        "--volume".to_owned(),
        run_socket_mount(container.queue, container.run)?,
    ]);
    if let Some(output) = container.build_output {
        let output = mount_path(Path::new(output))?;
        let volume = container
            .queue
            .build_output_volume(container.verification)?;
        args.extend(["--volume".to_owned(), format!("{volume}:{output}")]);
    }
    args.extend([
        "--workdir".to_owned(),
        format!("{dir}/{VERIFICATION_CLONE}"),
    ]);
    finish_args(&mut args, container.env, container.image, container.command)?;
    Ok(args)
}

/// What a verification's container runs for the verification command
/// `command`: `dagq relay verify` of the image's client, which starts the
/// forwarder, gives the command the proxy variables of [`verification_env`]
/// from the token's copy in `dir`, and runs it with `sh -c`.
pub fn verification_command(dir: &Path, command: &str) -> Vec<String> {
    [
        "dagq",
        "relay",
        "verify",
        "--token-file",
        &dir.join(VERIFICATION_TOKEN).to_string_lossy(),
        "--",
        "sh",
        "-c",
        command,
    ]
    .iter()
    .map(|arg| (*arg).to_owned())
    .collect()
}

/// The variables `dagq relay verify` gives a verification command from the
/// run `run` and the verification's token `token` (ADR-t2257-1 decision
/// 3): the standard proxy variables' URL on the forwarder
/// (`http://<run>:<token>@127.0.0.1:<port>`), which the broker opens to
/// the registries `dagq.toml` names alone, and the loopback left out of
/// the proxy. No URL of the queue service or the Claude API: the token
/// reaches neither.
pub fn verification_env(run: &str, token: &str) -> Vec<(String, String)> {
    let proxy = format!("http://{run}:{token}@127.0.0.1:{FORWARD_PORT}");
    container_env()
        .into_iter()
        .filter_map(|(key, value)| match key {
            "HTTPS_PROXY" | "HTTP_PROXY" | "https_proxy" | "http_proxy" => {
                Some((key.to_owned(), proxy.clone()))
            }
            "NO_PROXY" | "no_proxy" => Some((key.to_owned(), value)),
            _ => None,
        })
        .collect()
}

/// `path` as a mount's source or target: absolute, UTF-8, and without the
/// characters that separate a mount's parts.
fn mount_path(path: &Path) -> Result<String, String> {
    let text = path
        .to_str()
        .ok_or_else(|| format!("{} is not UTF-8", path.display()))?;
    if !path.is_absolute() || text.contains([',', ':']) || text.contains("..") {
        return Err(format!(
            "{text:?} cannot be mounted in a run's container (an absolute path without , : or ..)"
        ));
    }
    Ok(text.to_owned())
}

/// What the run's container runs: `dagq relay turn` of the image's client,
/// which marks the run's state as of a turn that has not ended, imports
/// main's bundle into the clone, starts the forwarder, runs `agent` (the
/// agent's arguments, [`AGENT`] first) with [`turn_env`], and writes the
/// run branch's bundle and the run's state at its end
/// ([`super::run_clone`]).
pub fn launcher_command(
    run_dir: &Path,
    clone: &Path,
    base: &str,
    branch: &str,
    agent: &[String],
) -> Vec<String> {
    let mut command: Vec<String> = [
        "dagq",
        "relay",
        "turn",
        "--run-dir",
        &run_dir.to_string_lossy(),
        "--clone",
        &clone.to_string_lossy(),
        "--base",
        base,
        "--branch",
        branch,
        "--",
    ]
    .iter()
    .map(|arg| (*arg).to_owned())
    .collect();
    command.extend(agent.iter().cloned());
    command
}

/// What the container of a facts-only turn runs: `dagq relay turn
/// --facts-only` of the image's client, which runs no agent and writes the
/// run branch's bundle, the clone's work and the run's state anew, as at a
/// turn's end; what the control side starts when the facts the last turn
/// left are not of its end.
pub fn facts_command(run_dir: &Path, clone: &Path, base: &str, branch: &str) -> Vec<String> {
    let mut command = launcher_command(run_dir, clone, base, branch, &[]);
    // The flag in place of the agent after `--`.
    command.pop();
    command.insert(3, "--facts-only".to_owned());
    command
}

/// The environment the container's launcher gives the agent, from the run
/// `run` and its token `token` read from [`TOKEN_FILE`] (ADR-t2114-2
/// decision 1, ADR-t2257-1 decision 3): the forwarder's URLs of
/// [`container_env`], the token as `ANTHROPIC_AUTH_TOKEN` and the run
/// in `ANTHROPIC_CUSTOM_HEADERS`, and the run and token in the standard
/// proxy variables' URL as a package manager sends them to the broker
/// (`http://<run>:<token>@127.0.0.1:<port>`). Every destination is the
/// forwarder on the container's loopback.
pub fn turn_env(run: &str, token: &str) -> Vec<(String, String)> {
    let local = format!("127.0.0.1:{FORWARD_PORT}");
    let proxy = format!("http://{run}:{token}@{local}");
    container_env()
        .into_iter()
        .map(|(key, value)| {
            let value = match key {
                "HTTPS_PROXY" | "HTTP_PROXY" | "https_proxy" | "http_proxy" => proxy.clone(),
                _ => value,
            };
            (key.to_owned(), value)
        })
        .chain([
            ("ANTHROPIC_AUTH_TOKEN".to_owned(), token.to_owned()),
            (
                "ANTHROPIC_CUSTOM_HEADERS".to_owned(),
                format!("{RUN_HEADER}: {run}"),
            ),
        ])
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only podman's own failure says the command did not run: the
    /// command's codes, 126 and 127 of a shell among them, and a signal are
    /// the command's.
    #[test]
    fn only_podman_s_own_exit_code_says_the_container_did_not_start() {
        assert!(podman_failed(Some(PODMAN_ERROR_EXIT)));
        for code in [Some(0), Some(1), Some(101), Some(126), Some(127), None] {
            assert!(!podman_failed(code), "{code:?}");
        }
    }
    use std::path::PathBuf;

    fn key() -> QueueKey {
        QueueKey::new("77067154921b9014").unwrap()
    }

    fn values<'a>(args: &'a [String], flag: &str) -> Vec<&'a str> {
        args.iter()
            .zip(args.iter().skip(1))
            .filter(|(f, _)| *f == flag)
            .map(|(_, value)| value.as_str())
            .collect()
    }

    /// A facts-only turn's container is hardened and named as the run's
    /// turns are, but mounts only the clone and the run directory: no
    /// socket of the run's relay to the queue broker, no build output, no
    /// worker cache, and no variable.
    #[test]
    fn a_facts_only_turn_s_container_mounts_only_the_clone_and_the_run_directory() {
        let queue = key();
        let clone = PathBuf::from("/q/clones/r1");
        let run_dir = PathBuf::from("/q/runs/r1");
        let command = facts_command(&run_dir, &clone, "abc", "dagq/r1");
        let args = facts_run_args(&FactsContainer {
            queue: &queue,
            run: "r1",
            image: "localhost/dagq-q-image:t",
            clone: &clone,
            run_dir: &run_dir,
            limits: ContainerLimits::default(),
            command: &command,
        })
        .unwrap();
        let text = args.join(" ");
        for part in [
            "run --rm --name dagq-77067154921b9014-run-r1",
            "--read-only",
            "--cap-drop all",
            "--security-opt no-new-privileges",
            "--network none",
            "--workdir /q/clones/r1",
            "localhost/dagq-q-image:t dagq relay turn --facts-only --run-dir /q/runs/r1",
        ] {
            assert!(text.contains(part), "{part}: {text}");
        }
        assert_eq!(
            values(&args, "--volume"),
            ["/q/clones/r1:/q/clones/r1", "/q/runs/r1:/q/runs/r1"]
        );
        assert!(values(&args, "--env").is_empty(), "{text}");
        // No agent reads a facts-only turn's standard input.
        assert!(!text.contains("--interactive"), "{text}");
        assert!(
            !text.contains("broker") && !text.contains("socket"),
            "{text}"
        );
        let refused = FactsContainer {
            clone: Path::new("/q/a:/x"),
            ..FactsContainer {
                queue: &queue,
                run: "r1",
                image: "i",
                clone: &clone,
                run_dir: &run_dir,
                limits: ContainerLimits::default(),
                command: &command,
            }
        };
        assert!(facts_run_args(&refused).is_err());
    }

    #[test]
    fn a_turn_s_container_is_hardened_and_mounts_only_the_run_s_parts() {
        let queue = key();
        let clone = PathBuf::from("/q/clones/r1");
        let run_dir = PathBuf::from("/q/runs/r1");
        let env = vec![("DAGQ_RUN_ID".to_owned(), "r1".to_owned())];
        let command = launcher_command(&run_dir, &clone, "abc", "dagq/r1", &["claude".into()]);
        let mut container = WorkerContainer {
            queue: &queue,
            run: "r1",
            image: "localhost/dagq-q-image:t",
            clone: &clone,
            run_dir: &run_dir,
            build_output: Some("/build"),
            worker_cache: &[],
            limits: ContainerLimits::default(),
            env: &env,
            unset_env: &[],
            command: &command,
        };
        let args = worker_run_args(&container).unwrap();
        let text = args.join(" ");
        for part in [
            "run --rm --name dagq-77067154921b9014-run-r1",
            "--user 10001:10001",
            "--userns keep-id:uid=10001,gid=10001",
            "--read-only",
            "--cap-drop all",
            "--security-opt no-new-privileges",
            "--memory 2048m",
            "--cpus 2",
            "--pids-limit 2048",
            "--network none",
            "--interactive --workdir /q/clones/r1",
            "--env DAGQ_RUN_ID=r1",
            "localhost/dagq-q-image:t dagq relay turn --run-dir /q/runs/r1 --clone /q/clones/r1 \
             --base abc --branch dagq/r1 -- claude",
        ] {
            assert!(text.contains(part), "{part}: {text}");
        }
        assert_eq!(
            values(&args, "--volume"),
            [
                "/q/clones/r1:/q/clones/r1",
                "/q/runs/r1:/q/runs/r1",
                "dagq-77067154921b9014-socket-r1:/run/dagq-broker:z",
                "dagq-77067154921b9014-build-r1:/build",
            ]
        );
        assert!(!text.contains(".sock:") || text.contains("/run/dagq-broker"));
        assert!(!text.contains("--privileged") && !text.contains("--cap-add"));
        // A facts-only turn runs no agent.
        assert_eq!(
            facts_command(&run_dir, &clone, "abc", "dagq/r1").join(" "),
            "dagq relay turn --facts-only --run-dir /q/runs/r1 --clone /q/clones/r1 --base abc \
             --branch dagq/r1"
        );
        // Without the setting, no build output volume.
        container.build_output = None;
        let args = worker_run_args(&container).unwrap();
        assert_eq!(values(&args, "--volume").len(), 3);
        // A path that would smuggle in a mount option, or a relative one,
        // is refused.
        for bad in ["/q/a,ro", "/q/a:/x", "relative", "/q/../etc"] {
            let path = PathBuf::from(bad);
            let refused = WorkerContainer {
                clone: &path,
                ..container.clone()
            };
            assert!(worker_run_args(&refused).is_err(), "{bad}");
        }
        assert!(
            worker_run_args(&WorkerContainer {
                build_output: Some("/b:/c"),
                ..container.clone()
            })
            .is_err()
        );
        // A variable the image sets is unset before the container's own
        // are given; one that is no variable's name is refused.
        let unset = worker_run_args(&WorkerContainer {
            unset_env: &["RUSTC_WRAPPER"],
            ..container.clone()
        })
        .unwrap();
        assert_eq!(values(&unset, "--unsetenv"), ["RUSTC_WRAPPER"]);
        assert!(
            unset.iter().position(|arg| arg == "--unsetenv")
                < unset.iter().position(|arg| arg == "--env")
        );
        for bad in ["", "A=B"] {
            assert!(
                worker_run_args(&WorkerContainer {
                    unset_env: &[bad],
                    ..container.clone()
                })
                .is_err(),
                "{bad:?}"
            );
        }
        let bad_env = vec![("A=B".to_owned(), "x".to_owned())];
        assert!(
            worker_run_args(&WorkerContainer {
                env: &bad_env,
                ..container
            })
            .is_err()
        );
    }

    /// The directories `[container.worker_cache]` lists are mounted in a
    /// turn's container from the queue's own volumes, one for each; a
    /// directory that would smuggle in a mount option is refused as an
    /// error of the setting, and no container is made.
    #[test]
    fn a_turn_s_container_mounts_the_queue_s_worker_cache_at_the_listed_directories() {
        let (a, b) = (key(), QueueKey::new("0123456789abcdef").unwrap());
        let clone = PathBuf::from("/q/clones/r1");
        let run_dir = PathBuf::from("/q/runs/r1");
        let command = vec!["claude".to_owned()];
        let dirs = vec!["/cache/registry".to_owned(), "/cache/compiler".to_owned()];
        let container = WorkerContainer {
            queue: &a,
            run: "r1",
            image: "localhost/dagq-q-image:t",
            clone: &clone,
            run_dir: &run_dir,
            build_output: None,
            worker_cache: &dirs,
            limits: ContainerLimits::default(),
            env: &[],
            unset_env: &[],
            command: &command,
        };
        let args = worker_run_args(&container).unwrap();
        let volumes = values(&args, "--volume");
        assert_eq!(
            volumes[3..],
            [
                format!(
                    "{}:/cache/registry",
                    a.worker_cache_volume_of("/cache/registry")
                ),
                format!(
                    "{}:/cache/compiler",
                    a.worker_cache_volume_of("/cache/compiler")
                ),
            ]
        );
        // Another queue's turn mounts its own volumes, never these.
        let other = worker_run_args(&WorkerContainer {
            queue: &b,
            ..container.clone()
        })
        .unwrap();
        for volume in values(&other, "--volume") {
            assert!(!volume.starts_with(&a.worker_cache_volume()), "{volume}");
        }
        assert!(
            values(&other, "--volume")[3].starts_with(&format!("{}-", b.worker_cache_volume()))
        );
        for bad in ["relative", "/a,ro", "/a:/b", "/q/../etc"] {
            let dirs = vec![bad.to_owned()];
            let why = worker_run_args(&WorkerContainer {
                worker_cache: &dirs,
                ..container.clone()
            })
            .unwrap_err();
            assert!(
                why.starts_with("[container.worker_cache] of dagq.toml:"),
                "{bad}: {why}"
            );
        }
    }

    #[test]
    fn a_verification_s_container_is_hardened_and_mounts_only_its_dir_materials_socket_and_output()
    {
        let queue = key();
        let dir = PathBuf::from("/q/verification/r1-v1");
        let materials = PathBuf::from("/q/runs/r1/verification-materials");
        let env = vec![("DAGQ_RUN_ID".to_owned(), "r1".to_owned())];
        let command = verification_command(&dir, "cargo test");
        let mut container = VerificationContainer {
            queue: &queue,
            run: "r1",
            verification: "r1-v1",
            image: "localhost/dagq-q-image:t",
            dir: &dir,
            materials: &materials,
            build_output: Some("/build"),
            limits: ContainerLimits::default(),
            env: &env,
            command: &command,
        };
        let args = verification_run_args(&container).unwrap();
        let text = args.join(" ");
        for part in [
            "run --rm --name dagq-77067154921b9014-verify-r1-v1",
            "--user 10001:10001",
            "--read-only",
            "--cap-drop all",
            "--security-opt no-new-privileges",
            "--memory 2048m --cpus 2 --pids-limit 2048",
            "--network none",
            "--workdir /q/verification/r1-v1/clone",
            "localhost/dagq-q-image:t dagq relay verify --token-file \
             /q/verification/r1-v1/token -- sh -c cargo test",
        ] {
            assert!(text.contains(part), "{part}: {text}");
        }
        assert_eq!(
            values(&args, "--volume"),
            [
                "/q/verification/r1-v1:/q/verification/r1-v1",
                "/q/runs/r1/verification-materials:/q/runs/r1/verification-materials:ro",
                "dagq-77067154921b9014-socket-r1:/run/dagq-broker:z",
                "dagq-77067154921b9014-build-r1-v1:/build",
            ]
        );
        // No worker cache of any queue, nor any cache of the host's.
        assert!(!text.contains("-cache"), "{text}");
        container.build_output = None;
        assert_eq!(
            values(&verification_run_args(&container).unwrap(), "--volume").len(),
            3
        );
        for bad in ["/q/v,ro", "relative", "/q/../etc"] {
            let path = PathBuf::from(bad);
            let refused = VerificationContainer {
                dir: &path,
                ..container.clone()
            };
            assert!(verification_run_args(&refused).is_err(), "{bad}");
        }
    }

    #[test]
    fn only_a_wrapper_that_names_sccache_is_taken_out_of_a_verification_s_env() {
        let pairs = |items: &[(&str, &str)]| -> Vec<(String, String)> {
            items
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect()
        };
        let mut env = pairs(&[
            ("RUSTC_WRAPPER", "/usr/local/bin/sccache"),
            ("SCCACHE_SERVER_PORT", "4300"),
            ("CARGO_BUILD_JOBS", "2"),
        ]);
        let removed = without_sccache_wrapper(&mut env).unwrap();
        assert_eq!(removed.port, 4300);
        assert_eq!(
            env,
            pairs(&[("SCCACHE_SERVER_PORT", "4300"), ("CARGO_BUILD_JOBS", "2")])
        );
        // Another wrapper, or none, is the project's and stays.
        for kept in [
            pairs(&[("RUSTC_WRAPPER", "/opt/bin/cachepot"), ("A", "1")]),
            pairs(&[("A", "1")]),
        ] {
            let mut env = kept.clone();
            assert_eq!(without_sccache_wrapper(&mut env), None);
            assert_eq!(env, kept);
        }
    }

    #[test]
    fn a_verification_s_token_is_given_to_the_proxies_alone() {
        let env = verification_env("r1", "tok");
        let get = |name: &str| {
            env.iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.as_str())
        };
        for proxy in ["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy"] {
            assert_eq!(get(proxy), Some("http://r1:tok@127.0.0.1:47310"), "{proxy}");
        }
        assert!(get("NO_PROXY").unwrap().contains("127.0.0.1"));
        for absent in [
            "ANTHROPIC_AUTH_TOKEN",
            "ANTHROPIC_BASE_URL",
            crate::domain::queue_service::URL_ENV,
        ] {
            assert_eq!(get(absent), None, "{absent}");
        }
    }

    #[test]
    fn a_clone_s_turn_runs_only_in_its_container_and_a_host_run_s_on_the_host() {
        use crate::domain::container_runtime::Runtime;
        assert_eq!(
            turn_place(true, Some(Runtime::Container)),
            Ok(TurnPlace::Container)
        );
        let back = turn_place(true, Some(Runtime::Host)).unwrap_err();
        assert!(back.contains("sets the worker to the host now"), "{back}");
        let unread = turn_place(true, None).unwrap_err();
        assert!(unread.contains("reads no runtime"), "{unread}");
        for worker in [None, Some(Runtime::Host), Some(Runtime::Container)] {
            assert_eq!(turn_place(false, worker), Ok(TurnPlace::Host), "{worker:?}");
        }
    }

    #[test]
    fn a_clone_s_transcripts_are_in_its_run_s_container_home_named_after_the_clone() {
        let clone = PathBuf::from("/q/clones/r1");
        assert_eq!(
            container_transcripts(&clone),
            Some(ContainerTranscripts {
                home: PathBuf::from("/q/runs/r1/container-home"),
                cwd: clone,
            })
        );
        // A run on the host reads the host's transcripts.
        assert_eq!(
            container_transcripts(Path::new("/q/runs/r1/worktree")),
            None
        );
        assert_eq!(container_transcripts(Path::new("/elsewhere")), None);
    }

    #[test]
    fn the_agent_is_given_the_token_from_its_file_and_every_destination_is_the_forwarder() {
        let env = turn_env("r1", "tok");
        let get = |name: &str| {
            env.iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.as_str())
        };
        assert_eq!(get("ANTHROPIC_AUTH_TOKEN"), Some("tok"));
        assert_eq!(
            get("ANTHROPIC_BASE_URL"),
            Some("http://127.0.0.1:47310/anthropic")
        );
        assert_eq!(get("ANTHROPIC_CUSTOM_HEADERS"), Some("x-dagq-run: r1"));
        assert_eq!(
            get(crate::domain::queue_service::URL_ENV),
            Some("http://127.0.0.1:47310")
        );
        for proxy in ["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy"] {
            assert_eq!(get(proxy), Some("http://r1:tok@127.0.0.1:47310"), "{proxy}");
        }
        assert!(get("NO_PROXY").unwrap().contains("127.0.0.1"));
        // Every URL is the container's loopback.
        for (key, value) in &env {
            if value.starts_with("http") {
                assert!(value.contains("127.0.0.1:47310"), "{key}={value}");
            }
        }
    }
}
