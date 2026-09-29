//! The broker's container on a real Podman (docs/design/broker.md
//! "container and Podman machine", ADR-t827-3). Every test here needs
//! podman on the host, inits and starts dagq's own machine when it has to,
//! and stops it again at the end, so they are `#[ignore]` like the e2e
//! tests: `cargo test --locked --test it broker_podman:: -- --ignored`.
//! The first run builds the image in the machine and takes minutes.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use dagq::application::broker::BrokerFailure;
use dagq::application::broker::{
    MACHINE, MachineSpec, MachineState, Podman, container_name, ensure_machine, machine_status,
    release_machine,
};
use dagq::application::broker_admin::AuditQuery;
use dagq::compose::{
    BrokerStartOptions, broker_audit, broker_logs, broker_start, broker_status, broker_stop,
};
use dagq::infrastructure::broker_podman::{FileLock, PodmanCli, free_port, get_health};
use dagq::infrastructure::location::{QueueLocation, data_home};

use crate::common::{Bounded, on_timeout, within};

/// Long enough for the first build of the image in the smallest machine.
const BUILD_LIMIT: Duration = Duration::from_secs(3600);

fn podman() -> PodmanCli {
    PodmanCli::resolve(None).expect("these tests need podman on PATH (brew install podman)")
}

fn host_lock() -> FileLock {
    FileLock::machine(&data_home().unwrap())
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .bounded_status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

/// A throwaway repository and its queue's paths, under the temp dir
/// (which dagq's machine mounts by default).
fn queue(root: &Path) -> QueueLocation {
    let repo = root.join("dagq-smoke");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(
        &repo,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "init",
        ],
    );
    let common = repo.join(".git").canonicalize().unwrap();
    let root = root.canonicalize().unwrap();
    QueueLocation::for_repository_in(&common, &root.join("data"), &root.join("home"))
}

/// A non-loopback IPv4 address of this host, if it has one: the address a
/// UDP socket would send from (connecting a UDP socket sends nothing).
fn lan_address() -> Option<Ipv4Addr> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect((Ipv4Addr::new(192, 0, 2, 1), 9)).ok()?;
    match socket.local_addr().ok()?.ip() {
        IpAddr::V4(ip) if !ip.is_loopback() && !ip.is_unspecified() => Some(ip),
        _ => None,
    }
}

#[test]
#[ignore = "needs podman; inits and starts dagq's own machine"]
fn the_machine_is_made_ready_idempotently_from_each_state() {
    let _test = within(BUILD_LIMIT, "dagq's machine to init, start and stop");
    let (podman, lock, spec) = (podman(), host_lock(), MachineSpec::default());
    let before = machine_status(&podman, MACHINE).unwrap();
    eprintln!("dagq's machine was {:?}", before.state);
    // From whatever state it is in (missing on a new host, else stopped or
    // running), then again from running.
    let first = ensure_machine(&podman, &lock, &spec).unwrap();
    assert_eq!(first.initialized, before.state == MachineState::Missing);
    assert_eq!(
        machine_status(&podman, MACHINE).unwrap().state,
        MachineState::Running
    );
    let again = ensure_machine(&podman, &lock, &spec).unwrap();
    assert!(!again.initialized && !again.started, "{again:?}");
    // Released twice: stopped once (when no container runs on it).
    let stopped = release_machine(&podman, &lock, MACHINE).unwrap();
    assert!(!release_machine(&podman, &lock, MACHINE).unwrap());
    if stopped {
        assert_eq!(
            machine_status(&podman, MACHINE).unwrap().state,
            MachineState::Stopped
        );
        // From stopped: started, not inited.
        let from_stopped = ensure_machine(&podman, &lock, &spec).unwrap();
        assert!(!from_stopped.initialized && from_stopped.started);
        assert!(release_machine(&podman, &lock, MACHINE).unwrap());
    }
    // Only dagq's machine was touched: its resources are the fewest.
    let listing = podman
        .run(&[
            "machine".into(),
            "inspect".into(),
            "--format".into(),
            "{{.Resources.CPUs}} {{.Resources.Memory}} {{.Resources.DiskSize}}".into(),
            MACHINE.into(),
        ])
        .unwrap();
    assert!(listing.success, "{}", listing.stderr);
    assert_eq!(listing.stdout.trim(), "1 1024 10");
}

#[test]
#[ignore = "needs podman; builds the image and runs the broker in dagq's machine"]
fn the_broker_runs_in_its_container_and_answers_on_loopback_only() {
    let _test = within(BUILD_LIMIT, "the broker to build, start, answer and stop");
    let dir = tempfile::tempdir().unwrap();
    let location = queue(dir.path());
    let container = container_name(&location.hash());
    let cleanup = {
        let location = location.clone();
        on_timeout(Duration::from_secs(120), "stop the broker", move || {
            let _ = broker_stop(&location, None);
        })
    };
    let port = free_port().unwrap();
    let options = BrokerStartOptions {
        port: Some(port),
        podman: None,
        cwd: dir.path().to_path_buf(),
    };
    let started = broker_start(&location, &options).unwrap();
    eprintln!("{started:#}");
    assert_eq!(started["state"], "running");
    assert_eq!(started["start"]["container_outcome"]["created"], true);
    assert_eq!(started["start"]["health"]["status"], "ok");
    // Built from the material this dagq embeds, as this dagq's build.
    assert_eq!(
        started["start"]["image"],
        dagq::infrastructure::broker_image::image()
    );
    assert_eq!(started["start"]["health"]["build"], dagq::VERSION);
    assert_eq!(started["start"]["build_matches"], true);
    // Again: nothing is made.
    let again = broker_start(&location, &options).unwrap();
    assert_eq!(again["start"]["image_built"], false);
    assert_eq!(again["start"]["container_outcome"]["created"], false);
    assert_eq!(again["start"]["machine"]["started"], false);

    // Health on 127.0.0.1.
    let health = get_health(
        SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
        Duration::from_secs(5),
    )
    .unwrap();
    assert_eq!(health.status, "ok");
    assert_eq!(health.protocol, 1);
    // Not on the LAN address.
    match lan_address() {
        Some(ip) => {
            let error = get_health(SocketAddr::from((ip, port)), Duration::from_secs(5))
                .expect_err("the broker must not answer on the LAN address");
            eprintln!("{ip}:{port}: {error}");
        }
        None => eprintln!("this host has no LAN address; the LAN check was not made"),
    }
    // What the container has: the mounts, no socket, no host environment.
    let inspected = podman()
        .run(&[
            "--connection".into(),
            MACHINE.into(),
            "container".into(),
            "inspect".into(),
            "--format".into(),
            "json".into(),
            container.clone(),
        ])
        .unwrap();
    assert!(inspected.success, "{}", inspected.stderr);
    let inspected: serde_json::Value = serde_json::from_str(&inspected.stdout).unwrap();
    let mounts: Vec<String> = inspected[0]["Mounts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|mount| mount["Destination"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(mounts.len(), 7, "{mounts:?}");
    for mount in &mounts {
        for forbidden in [".sock", ".ssh", ".aws", "queue.db"] {
            assert!(!mount.contains(forbidden), "{mount}");
        }
        assert!(
            mount.starts_with(location.queue_dir.to_str().unwrap())
                || mount.starts_with(location.git_common_dir.as_ref().unwrap().to_str().unwrap()),
            "{mount}"
        );
    }
    let env = inspected[0]["Config"]["Env"].to_string();
    for secret in [
        "AWS_",
        "GH_TOKEN",
        "GITHUB_TOKEN",
        "ANTHROPIC",
        "SSH_AUTH_SOCK",
    ] {
        assert!(!env.contains(secret), "{env}");
    }
    let ports = inspected[0]["NetworkSettings"]["Ports"].to_string();
    assert!(ports.contains("127.0.0.1"), "{ports}");
    assert!(!ports.contains("0.0.0.0"), "{ports}");

    let status = broker_status(&location, None).unwrap();
    assert_eq!(status["state"], "running", "{status:#}");
    // The logs and the audit read what the running broker wrote.
    let logs = broker_logs(&location, None, 50).unwrap();
    assert_eq!(logs["container"], container.as_str(), "{logs:#}");
    assert_eq!(logs["tail"], 50);
    let audit = broker_audit(&location, &AuditQuery::default()).unwrap();
    let entries = audit["entries"].as_array().unwrap();
    assert!(
        entries.iter().any(|entry| entry["op"] == "health"),
        "{audit:#}"
    );
    assert_eq!(audit["skipped"], 0, "{audit:#}");

    // Stopped: the container, then the machine (nothing else runs on it).
    let stopped = broker_stop(&location, None).unwrap();
    assert_eq!(stopped["stop"]["container_stopped"], true);
    assert!(
        get_health(
            SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
            Duration::from_secs(2)
        )
        .is_err()
    );
    let again = broker_stop(&location, None).unwrap();
    assert_eq!(again["stop"]["container_stopped"], false);
    let status = broker_status(&location, None).unwrap();
    assert_ne!(status["state"], "running", "{status:#}");
    // The logs start nothing: the container is gone and the machine stopped.
    let error = broker_logs(&location, None, 50).unwrap_err();
    let code = error.downcast_ref::<BrokerFailure>().unwrap().code.as_str();
    assert!(
        ["machine_stopped", "container_missing"].contains(&code),
        "{error:#}"
    );
    drop(cleanup);
}
