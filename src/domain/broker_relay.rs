//! The path from a run's container to the queue broker ([Container
//! runtime], ADR-t2114-3 decisions 2 to 4): the container has no network
//! (`--network none`), so a forwarder inside it takes `127.0.0.1:`
//! [`FORWARD_PORT`] and passes each connection to the run's unix socket,
//! which the run's relay in the dagq machine binds and passes on to the
//! broker's port on the host. The relay and the forwarder hold no secret
//! and judge nothing: they carry bytes both ways, and the broker checks the
//! run token and the destination.
//!
//! What is decided here: the names and the mounts of the run's socket and
//! relay ([`relay_run_args`], [`run_socket_mount`]), the environment that
//! points a run's tools at the forwarder ([`container_env`]), and what the
//! supervisor does after a look at a relay ([`relay_action`]).
//!
//! [Container runtime]: ../../docs/design/container-runtime.md

use serde::{Deserialize, Serialize};

use super::podman_machine::QueueKey;

/// The port of `127.0.0.1` the forwarder inside a run's container listens
/// on. The container has only its loopback, so any port is free; it is
/// fixed so the environment a container starts with ([`container_env`])
/// can name it.
pub const FORWARD_PORT: u16 = 47_310;
/// Where the run's socket volume is mounted, in the relay's container and
/// in the run's.
pub const SOCKET_DIR: &str = "/run/dagq-broker";
/// The run's socket in [`SOCKET_DIR`].
pub const SOCKET_FILE: &str = "broker.sock";
/// The mode of the run's socket: only its owner opens it.
pub const SOCKET_MODE: u32 = 0o600;
/// The uid of the non-root user a run's container runs as (ADR-t2113-1
/// decision 6), the one the runtime's base image creates: the run's socket
/// is this user's, so only the run's own process opens it.
pub const CONTAINER_UID: u32 = 10001;
/// The name a container in the dagq machine reaches the host by: gvproxy
/// passes it to the host's loopback, where the broker listens.
pub const HOST_GATEWAY: &str = "host.containers.internal";
/// The label every relay container of a queue carries, valued with the
/// queue's key.
pub const QUEUE_LABEL: &str = "dagq.queue";
/// The label that names the run a relay container is for.
pub const RUN_LABEL: &str = "dagq.relay.run";
/// The label that names the broker's port a relay container passes to: a
/// broker started again listens on another, and the relay is started
/// again towards it.
pub const TARGET_LABEL: &str = "dagq.relay.target";

/// The relay of one run: what the control side records when it opens it,
/// and what it is started again from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelaySpec {
    /// The run's ID.
    pub run: String,
    /// The image the relay runs (the run's: it carries the Linux dagq
    /// client, whose `relay serve` is the relay).
    pub image: String,
    /// The uid the run's socket is given to: [`CONTAINER_UID`].
    pub uid: u32,
}

/// The path of the run's socket inside its container and its relay's.
pub fn socket_path() -> String {
    format!("{SOCKET_DIR}/{SOCKET_FILE}")
}

/// The arguments after `podman --connection dagq` that start the relay of
/// `spec` in the machine, passing to the broker's `port` on the host. The
/// relay is the only container that mounts the run's socket volume beside
/// the run's own, with the shared SELinux label (`:z`); it runs as root in
/// its container only to give the socket to the run's uid ([`SOCKET_MODE`]),
/// with every capability dropped but `CHOWN`, a read-only root filesystem
/// and no privilege escalation. It holds no token and no host file.
pub fn relay_run_args(
    queue: &QueueKey,
    spec: &RelaySpec,
    port: u16,
) -> Result<Vec<String>, String> {
    let container = queue.relay_container(&spec.run)?;
    let volume = queue.socket_volume(&spec.run)?;
    let mut args: Vec<String> = [
        "run",
        "--detach",
        "--name",
        &container,
        "--label",
        &format!("{QUEUE_LABEL}={}", queue.as_str()),
        "--label",
        &format!("{RUN_LABEL}={}", spec.run),
        "--label",
        &format!("{TARGET_LABEL}={port}"),
        "--user",
        "0:0",
        "--read-only",
        "--cap-drop",
        "all",
        "--cap-add",
        "CHOWN",
        "--security-opt",
        "no-new-privileges",
        "--volume",
        &format!("{volume}:{SOCKET_DIR}:z"),
        &spec.image,
    ]
    .iter()
    .map(|arg| (*arg).to_owned())
    .collect();
    args.extend(relay_command(&format!("{HOST_GATEWAY}:{port}"), spec.uid));
    Ok(args)
}

/// The relay's command line in its container: `dagq relay serve`.
pub fn relay_command(target: &str, uid: u32) -> Vec<String> {
    [
        "dagq",
        "relay",
        "serve",
        "--socket",
        &socket_path(),
        "--target",
        target,
        "--owner",
        &uid.to_string(),
    ]
    .iter()
    .map(|arg| (*arg).to_owned())
    .collect()
}

/// The `--volume` of the run `run`'s container that mounts its socket
/// volume at [`SOCKET_DIR`] with the shared SELinux label: only that
/// run's container is given it, so no other run sees the socket.
pub fn run_socket_mount(queue: &QueueKey, run: &str) -> Result<String, String> {
    Ok(format!("{}:{SOCKET_DIR}:z", queue.socket_volume(run)?))
}

/// The forwarder's command line in a run's container: `dagq relay
/// forward`, from `127.0.0.1:`[`FORWARD_PORT`] to the run's socket.
pub fn forward_command() -> Vec<String> {
    [
        "dagq",
        "relay",
        "forward",
        "--port",
        &FORWARD_PORT.to_string(),
        "--socket",
        &socket_path(),
    ]
    .iter()
    .map(|arg| (*arg).to_owned())
    .collect()
}

/// The environment of a run's container that points its tools at the
/// forwarder (ADR-t2114-3 decision 3, ADR-t2257-1 decision 3): the dagq
/// client's broker ([`crate::domain::queue_service::URL_ENV`]), Claude
/// Code's `ANTHROPIC_BASE_URL`, and the standard proxy variables a package
/// manager reads (`HTTPS_PROXY`, `HTTP_PROXY` and their lower-case forms),
/// with the loopback kept off the proxy (`NO_PROXY`). No tool's own setting
/// is here: a tool that reads no proxy variable cannot go out, which is the
/// isolation's (the container has no DNS either).
pub fn container_env() -> Vec<(&'static str, String)> {
    let url = format!("http://127.0.0.1:{FORWARD_PORT}");
    let local = "127.0.0.1,localhost".to_owned();
    vec![
        (super::queue_service::URL_ENV, url.clone()),
        ("ANTHROPIC_BASE_URL", url.clone()),
        ("HTTPS_PROXY", url.clone()),
        ("HTTP_PROXY", url.clone()),
        ("https_proxy", url.clone()),
        ("http_proxy", url),
        ("NO_PROXY", local.clone()),
        ("no_proxy", local),
    ]
}

/// A look at a run's relay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelaySeen {
    pub run: String,
    /// Its container runs.
    pub running: bool,
    /// The broker's port it passes to ([`TARGET_LABEL`]).
    pub target_port: Option<u16>,
    /// A request through its socket reached the broker's health; `None`
    /// when not looked at (it does not run).
    pub reaches: Option<bool>,
}

/// What the supervisor does about one relay after a look.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayAction {
    /// It runs towards the broker and reaches it.
    Keep,
    /// It does not run, or passes to a port the broker left: start it
    /// again towards the broker's port.
    Restart,
    /// It runs towards the broker's port and does not reach it, while the
    /// broker answers on the host: the machine's gvproxy is taken to be
    /// down, and the machine is started again (ADR-t2114-3 decision 4).
    RestartMachine,
}

/// What to do about the relay `seen`, the broker listening on `port` and
/// answering.
pub fn relay_action(seen: &RelaySeen, port: u16) -> RelayAction {
    if !seen.running || seen.target_port != Some(port) {
        RelayAction::Restart
    } else if seen.reaches == Some(false) {
        RelayAction::RestartMachine
    } else {
        RelayAction::Keep
    }
}

/// The port of a broker's `127.0.0.1:<port>` address.
pub fn address_port(address: &str) -> Option<u16> {
    address.rsplit_once(':')?.1.parse().ok()
}

/// The supervisor could not keep a run's relay running: the inbox's
/// attention (`reason`: `look_failed`, `start_failed` or `restart_limit`,
/// `message`, the `supervisor`'s token) until every relay runs again.
/// Meanwhile the container runtime cannot be used.
pub const QUEUE_RELAY_DOWN: &str = crate::domain::event_kind::EventKind::QueueRelayDown.as_str();
/// Every relay runs again after a [`QUEUE_RELAY_DOWN`].
pub const QUEUE_RELAY_RUNNING: &str =
    crate::domain::event_kind::EventKind::QueueRelayRunning.as_str();
/// The supervisor started a run's relay again (`run`, `port`,
/// `machine_restarted`: the machine was started again first, the
/// `supervisor`'s token).
pub const QUEUE_RELAY_STARTED: &str =
    crate::domain::event_kind::EventKind::QueueRelayStarted.as_str();
/// The kinds whose latest says whether the attention stands.
pub const QUEUE_RELAY_ATTENTION_KINDS: [&str; 2] = [QUEUE_RELAY_DOWN, QUEUE_RELAY_RUNNING];

/// Whether the attention stands: the latest of
/// [`QUEUE_RELAY_ATTENTION_KINDS`] is [`QUEUE_RELAY_DOWN`].
pub fn attention_stands(latest: Option<&str>) -> bool {
    latest == Some(QUEUE_RELAY_DOWN)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(run: &str) -> RelaySpec {
        RelaySpec {
            run: run.to_owned(),
            image: "localhost/dagq-worker:abc".to_owned(),
            uid: CONTAINER_UID,
        }
    }

    #[test]
    fn a_relay_mounts_only_its_run_s_socket_with_the_shared_label_and_gives_it_to_the_uid() {
        let queue = QueueKey::new("77067154921b9014").unwrap();
        let args = relay_run_args(&queue, &spec("r1"), 4321).unwrap();
        let volumes: Vec<&String> = args
            .iter()
            .zip(args.iter().skip(1))
            .filter(|(flag, _)| *flag == "--volume")
            .map(|(_, value)| value)
            .collect();
        assert_eq!(
            volumes,
            ["dagq-77067154921b9014-socket-r1:/run/dagq-broker:z"]
        );
        let text = args.join(" ");
        for part in [
            "--name dagq-77067154921b9014-relay-r1",
            "--label dagq.queue=77067154921b9014",
            "--label dagq.relay.run=r1",
            "--label dagq.relay.target=4321",
            "--cap-drop all --cap-add CHOWN",
            "--read-only",
            "no-new-privileges",
            "localhost/dagq-worker:abc dagq relay serve --socket /run/dagq-broker/broker.sock \
             --target host.containers.internal:4321 --owner 10001",
        ] {
            assert!(text.contains(part), "{part}: {text}");
        }
        // No network flag: the relay needs the machine's default to reach
        // the host. No secret, no host path.
        assert!(!text.contains("--network"), "{text}");
        assert!(
            !text.contains("TOKEN") && !text.contains("/Users"),
            "{text}"
        );
        // The run's container mounts its own socket volume, and another
        // run's is another volume.
        assert_eq!(
            run_socket_mount(&queue, "r1").unwrap(),
            "dagq-77067154921b9014-socket-r1:/run/dagq-broker:z"
        );
        assert_ne!(
            run_socket_mount(&queue, "r1"),
            run_socket_mount(&queue, "r2")
        );
        assert!(run_socket_mount(&queue, "../x").is_err());
        assert!(relay_run_args(&queue, &spec("a b"), 1).is_err());
    }

    #[test]
    fn a_run_s_tools_are_pointed_at_the_forwarder_by_standard_variables() {
        let env = container_env();
        let url = format!("http://127.0.0.1:{FORWARD_PORT}");
        for name in [
            crate::domain::queue_service::URL_ENV,
            "ANTHROPIC_BASE_URL",
            "HTTPS_PROXY",
            "https_proxy",
            "HTTP_PROXY",
            "http_proxy",
        ] {
            assert!(
                env.iter().any(|(key, value)| *key == name && *value == url),
                "{name}"
            );
        }
        assert!(
            env.iter()
                .any(|(key, value)| *key == "NO_PROXY" && value.contains("127.0.0.1"))
        );
        // No tool's own setting.
        assert!(!env.iter().any(|(key, _)| key.starts_with("CARGO")));
        assert_eq!(
            forward_command().join(" "),
            format!(
                "dagq relay forward --port {FORWARD_PORT} --socket /run/dagq-broker/broker.sock"
            )
        );
    }

    #[test]
    fn a_relay_is_kept_started_again_or_the_machine_is_started_again() {
        let seen = |running, target_port, reaches| RelaySeen {
            run: "r1".to_owned(),
            running,
            target_port,
            reaches,
        };
        assert_eq!(
            relay_action(&seen(true, Some(9), Some(true)), 9),
            RelayAction::Keep
        );
        assert_eq!(
            relay_action(&seen(true, Some(9), None), 9),
            RelayAction::Keep
        );
        assert_eq!(
            relay_action(&seen(false, Some(9), None), 9),
            RelayAction::Restart
        );
        // The broker was started again on another port.
        assert_eq!(
            relay_action(&seen(true, Some(8), Some(true)), 9),
            RelayAction::Restart
        );
        assert_eq!(
            relay_action(&seen(true, None, Some(true)), 9),
            RelayAction::Restart
        );
        assert_eq!(
            relay_action(&seen(true, Some(9), Some(false)), 9),
            RelayAction::RestartMachine
        );
        assert_eq!(address_port("127.0.0.1:4321"), Some(4321));
        assert_eq!(address_port("127.0.0.1"), None);
        assert!(attention_stands(Some(QUEUE_RELAY_DOWN)));
        assert!(!attention_stands(Some(QUEUE_RELAY_RUNNING)));
        assert!(!attention_stands(None));
    }
}
