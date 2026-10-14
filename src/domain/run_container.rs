//! The container a worker's turn runs in ([Container runtime], ADR-t2113-3
//! decision 1, ADR-t2113-1 decision 6, ADR-t2114-2 decision 1, ADR-t2114-3
//! decision 1): one container of the dagq machine per turn of a run whose
//! worker is set to the container, named after the run so that whatever
//! stops the run removes it ([`QueueKey::run_container`]).
//!
//! What is decided here: the arguments of `podman run` ([`worker_run_args`]:
//! a non-root user, a read-only root filesystem, no capability, no
//! privilege escalation, the limits of [`ContainerLimits`], no network, and
//! as mounts only the run's clone, its directory, its socket and its build
//! output), the command the container runs ([`launcher_command`], the
//! `dagq relay turn` of the image's client), and the environment that
//! command gives the agent from the run token's file ([`turn_env`]). The
//! token's value is never in the arguments: the container reads it from
//! [`TOKEN_FILE`] in the run directory.
//!
//! [Container runtime]: ../../docs/design/container-runtime.md

use std::path::Path;

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

/// The agent's executable on the image's `PATH`.
pub const AGENT: &str = "claude";

/// The label of a run's container, valued with its run's ID (beside
/// [`super::broker_relay::QUEUE_LABEL`]).
pub const RUN_LABEL: &str = "dagq.run";

/// The variable a turn's container is not given from `[run.env]`: a
/// compiler wrapper names a server of the host's, which the container
/// cannot reach (the sccache inside the container is a later part, ADR-t2114-4).
pub const WITHOUT_ENV: [&str; 1] = ["RUSTC_WRAPPER"];

/// The limits of each run's container (ADR-t2257-1 decision 6), from the
/// `[container]` table of the host-wide `host.toml` (`memory_mib`, `cpus`,
/// `pids`); a key left out keeps its default. The defaults are modest so
/// that they hold for any project on a machine of the default size
/// ([`super::podman_machine::DEFAULT_MACHINE_MEMORY_MIB`] and
/// [`super::podman_machine::DEFAULT_MACHINE_CPUS`]): half the machine's
/// memory, so a worker's container and a verification's fit at once, its
/// CPUs, and enough processes for a build's parallel jobs and a test
/// suite's threads. A project whose builds need more says so in the
/// host's `host.toml` (for this repository, the value its docs recommend),
/// with a machine made large enough.
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
    pub limits: ContainerLimits,
    /// The variables of the agent: none holds a secret.
    pub env: &'a [(String, String)],
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
/// are the clone, the run directory, the run's socket and the build
/// output: no Podman or Docker socket, no `$HOME` of the host, no landing
/// worktree and no verification's clone. Every path is checked to be
/// absolute and free of `,` and `:` so no mount option can be smuggled in.
pub fn worker_run_args(container: &WorkerContainer<'_>) -> Result<Vec<String>, String> {
    let name = container.queue.run_container(container.run)?;
    let clone = mount_path(container.clone)?;
    let run_dir = mount_path(container.run_dir)?;
    let user = format!("{CONTAINER_UID}:{CONTAINER_UID}");
    let mut args: Vec<String> = [
        "run",
        "--rm",
        "--name",
        &name,
        "--label",
        &format!(
            "{}={}",
            super::broker_relay::QUEUE_LABEL,
            container.queue.as_str()
        ),
        "--label",
        &format!("{RUN_LABEL}={}", container.run),
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
        &format!("{}m", container.limits.memory_mib),
        "--cpus",
        &container.limits.cpus.to_string(),
        "--pids-limit",
        &container.limits.pids.to_string(),
        "--network",
        "none",
        "--volume",
        &format!("{clone}:{clone}"),
        "--volume",
        &format!("{run_dir}:{run_dir}"),
        "--volume",
        &run_socket_mount(container.queue, container.run)?,
    ]
    .iter()
    .map(|arg| (*arg).to_owned())
    .collect();
    if let Some(dir) = container.build_output {
        let dir = mount_path(Path::new(dir))?;
        let volume = container.queue.build_output_volume(container.run)?;
        args.extend(["--volume".to_owned(), format!("{volume}:{dir}")]);
    }
    args.extend(["--workdir".to_owned(), clone.clone()]);
    for (key, value) in container.env {
        if key.is_empty() || key.contains('=') {
            return Err(format!("{key:?} cannot name a variable of the container"));
        }
        args.extend(["--env".to_owned(), format!("{key}={value}")]);
    }
    args.push(container.image.to_owned());
    args.extend(container.command.iter().cloned());
    Ok(args)
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
/// which imports main's bundle into the clone, starts the forwarder, runs
/// `agent` (the agent's arguments, [`AGENT`] first) with [`turn_env`], and
/// writes the run branch's bundle and the run's state at its end
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
            limits: ContainerLimits::default(),
            env: &env,
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
            "--workdir /q/clones/r1",
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
        let bad_env = vec![("A=B".to_owned(), "x".to_owned())];
        assert!(
            worker_run_args(&WorkerContainer {
                env: &bad_env,
                ..container
            })
            .is_err()
        );
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
