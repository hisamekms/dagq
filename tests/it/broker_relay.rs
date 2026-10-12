//! The path of a run's container to the queue broker (docs/design/
//! container-runtime.md, ADR-t2114-3): the run's socket and relay in the
//! dagq machine through a fake `podman` (only the run's relay and container
//! mount its socket volume, with `:z`, and the run's end removes both);
//! the supervisor that starts a relay again, starts the machine again when
//! a relay does not reach the broker, and tells the inbox when it cannot,
//! while nothing set to the container runs; and the whole path on the host
//! with the real broker and service: a client-mode `dagq` given the
//! broker's URL reaches the service through the forwarder, the run's
//! socket and the relay as its run's principal, and is refused as the
//! socket's client is without a valid run token. The real machine is in
//! the ignored test at the end.

use crate::common::service::{events, queue, start};
use crate::common::{self, Bounded, WithoutActor, cli::ok, shell_path};

use std::fs;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use dagq::application::broker_relay::RelayControl;
use dagq::application::queue_broker::{BrokerProbe, QueueBrokerControl};
use dagq::domain::broker_relay::{RelaySeen, RelaySpec, forward_command};
use dagq::domain::queue_service::{
    CREDENTIAL_FILE_ENV, Principal, SOCKET_ENV, ServiceState, URL_ENV,
};
use dagq::infrastructure::broker_relay::{PodmanRelays, forward_on};
use dagq::infrastructure::podman_machine::PodmanCli;
use dagq::infrastructure::queue_broker::{self as broker, SystemQueueBroker};
use dagq::infrastructure::queue_service as service;
use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(10);

/// A `podman` that keeps the containers and volumes of the dagq machine
/// in `state`, writes each command line to `state/calls`, fails a relay's
/// `run` while `state/fail-run` is there, and answers a relay's probe as
/// unreachable while `state/unreachable` is there. A machine's stop takes
/// its containers with it.
fn fake_podman(state: &Path) -> PathBuf {
    fs::create_dir_all(state.join("containers")).unwrap();
    fs::create_dir_all(state.join("volumes")).unwrap();
    let s = shell_path(state);
    let script = format!(
        r#"#!/bin/sh
S={s}
printf '%s\n' "$*" >> "$S/calls"
case "$1 $2" in
"machine list") printf '[{{"Name":"dagq","Running":true}}]' ;;
"machine stop") rm -f "$S"/containers/* ;;
"machine start") ;;
"system connection") printf '[{{"Name":"dagq","Default":true}}]' ;;
"--connection dagq")
  shift 2
  case "$1" in
  rm) rm -f "$S/containers/$4" ;;
  volume)
    case "$2" in
    create) eval "name=\${{$#}}"; touch "$S/volumes/$name" ;;
    exists) [ -f "$S/volumes/$3" ] ;;
    rm) rm "$S/volumes/$4" ;;
    esac ;;
  run)
    [ -f "$S/fail-run" ] && {{ echo "cannot run" >&2; exit 125; }}
    name=""; port=""; next=""
    for arg in "$@"; do
      case "$next" in
      name) name="$arg" ;;
      label) case "$arg" in dagq.relay.target=*) port="${{arg#*=}}" ;; esac ;;
      esac
      next=""
      [ "$arg" = --name ] && next=name
      [ "$arg" = --label ] && next=label
    done
    echo "true $port" > "$S/containers/$name" ;;
  container)
    eval "name=\${{$#}}"
    [ -f "$S/containers/$name" ] || exit 125
    cat "$S/containers/$name" ;;
  exec) [ "$2 $3" = "--user 10001" ] || exit 126; [ -f "$S/unreachable" ] && exit 1; exit 0 ;;
  esac ;;
*) exit 125 ;;
esac
"#
    );
    let program = state.join("podman");
    fs::write(&program, script).unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
    program
}

fn calls(state: &Path) -> Vec<String> {
    fs::read_to_string(state.join("calls"))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

const QUEUE: &str = "aaaaaaaaaaaaaaaa";

fn spec(run: &str) -> RelaySpec {
    RelaySpec {
        run: run.to_owned(),
        image: "localhost/dagq-run:t".to_owned(),
        uid: dagq::domain::broker_relay::CONTAINER_UID,
    }
}

/// Acceptance (1): each run gets its own socket volume, which only its
/// relay mounts (with `:z`, the socket given to the run's uid with mode
/// 0600 by the relay's command) and only the run's container is given; a
/// look finds each relay; the run's end removes its relay and volume and
/// leaves the other run's.
#[test]
fn each_run_gets_its_own_socket_and_relay_and_its_end_removes_them() {
    let _test = common::test();
    let dir = tempfile::tempdir().unwrap();
    let queue_dir = dir.path().join("data").join("dagq").join(QUEUE);
    fs::create_dir_all(&queue_dir).unwrap();
    let db = queue_dir.join("queue.db");
    fs::write(&db, "").unwrap();
    let state = dir.path().join("podman");
    let relays = PodmanRelays::of_queue(
        PodmanCli {
            program: fake_podman(&state),
        },
        &db,
        None,
    );
    relays.open(&spec("r1"), 4321).unwrap();
    relays.open(&spec("r2"), 4321).unwrap();
    let volume = |run: &str| format!("dagq-{QUEUE}-socket-{run}");
    let runs: Vec<String> = calls(&state)
        .into_iter()
        .filter(|call| call.starts_with("--connection dagq run "))
        .collect();
    assert_eq!(runs.len(), 2, "{runs:?}");
    for (call, run, other) in [(&runs[0], "r1", "r2"), (&runs[1], "r2", "r1")] {
        assert!(
            call.contains(&format!("--volume {}:/run/dagq-broker:z ", volume(run))),
            "{call}"
        );
        assert!(!call.contains(&volume(other)), "{call}");
        assert!(
            call.contains(
                "dagq relay serve --socket /run/dagq-broker/broker.sock \
                 --target host.containers.internal:4321 --owner 10001"
            ),
            "{call}"
        );
        assert!(!call.contains("--network"), "{call}");
    }
    // The run's container is given its own volume only.
    let queue = dagq::domain::podman_machine::QueueKey::new(QUEUE).unwrap();
    assert_eq!(
        dagq::domain::broker_relay::run_socket_mount(&queue, "r1").unwrap(),
        format!("{}:/run/dagq-broker:z", volume("r1"))
    );
    assert!(state.join("volumes").join(volume("r1")).is_file());
    let records = fs::metadata(queue_dir.join("relay")).unwrap();
    assert_eq!(records.permissions().mode() & 0o777, 0o700);

    let seen = relays.relays().unwrap();
    assert_eq!(
        seen,
        ["r1", "r2"].map(|run| RelaySeen {
            run: run.to_owned(),
            running: true,
            target_port: Some(4321),
            reaches: Some(true),
        })
    );

    relays.close("r1").unwrap();
    assert!(!state.join("volumes").join(volume("r1")).exists());
    assert!(
        !state
            .join("containers")
            .join(format!("dagq-{QUEUE}-relay-r1"))
            .exists()
    );
    assert!(state.join("volumes").join(volume("r2")).is_file());
    assert_eq!(
        relays
            .relays()
            .unwrap()
            .iter()
            .map(|relay| relay.run.as_str())
            .collect::<Vec<_>>(),
        ["r2"]
    );
    // Closing again is no error.
    relays.close("r1").unwrap();

    // A relay that does not start is not left open.
    fs::write(state.join("fail-run"), "").unwrap();
    assert!(relays.open(&spec("r3"), 4321).is_err());
    assert!(!state.join("volumes").join(volume("r3")).exists());
    fs::remove_file(state.join("fail-run")).unwrap();

    // The machine is started again under its lock; its relays went with
    // it, and a look finds them down.
    relays.restart_machine().unwrap();
    let tail = calls(&state);
    assert!(
        tail.iter().any(|call| call == "machine stop dagq")
            && tail.iter().any(|call| call == "machine start dagq"),
        "{tail:?}"
    );
    assert!(!relays.relays().unwrap()[0].running);
    relays.restart("r2", 5555).unwrap();
    assert_eq!(relays.relays().unwrap()[0].target_port, Some(5555));
    assert!(relays.open(&spec("../x"), 1).is_err());
}

/// The relays of a test's hands: what each look finds, whether a start
/// works, with the starts of relays and of the machine counted.
#[derive(Default)]
struct FakeRelays {
    seen: Mutex<Vec<RelaySeen>>,
    starts_fail: bool,
    restarts: Mutex<Vec<(String, u16)>>,
    machine_restarts: AtomicU32,
}

impl RelayControl for FakeRelays {
    fn open(&self, _: &RelaySpec, _: u16) -> anyhow::Result<()> {
        unreachable!("the supervisor opens no relay")
    }
    fn close(&self, _: &str) -> anyhow::Result<()> {
        unreachable!("the supervisor closes no relay")
    }
    fn relays(&self) -> anyhow::Result<Vec<RelaySeen>> {
        Ok(self.seen.lock().unwrap().clone())
    }
    fn restart(&self, run: &str, port: u16) -> anyhow::Result<()> {
        self.restarts.lock().unwrap().push((run.to_owned(), port));
        if self.starts_fail {
            anyhow::bail!("podman cannot start the relay");
        }
        for relay in self.seen.lock().unwrap().iter_mut() {
            if relay.run == run {
                relay.running = true;
                relay.target_port = Some(port);
                relay.reaches = Some(true);
            }
        }
        Ok(())
    }
    fn restart_machine(&self) -> anyhow::Result<()> {
        self.machine_restarts.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// A broker that runs this build at `127.0.0.1:4321`.
struct RunningBroker;

impl QueueBrokerControl for RunningBroker {
    fn probe(&self) -> BrokerProbe {
        BrokerProbe {
            state: ServiceState::Running,
            address: Some("127.0.0.1:4321".to_owned()),
            pid: Some(4242),
            build: Some(dagq::VERSION.to_owned()),
            build_matches: Some(true),
            started_at: None,
            error: None,
        }
    }
    fn start(&self, _: Duration) -> anyhow::Result<BrokerProbe> {
        Ok(self.probe())
    }
    fn stop(&self, _: Duration) -> anyhow::Result<Option<u32>> {
        Ok(None)
    }
}

fn relay(run: &str, running: bool, target_port: Option<u16>, reaches: Option<bool>) -> RelaySeen {
    RelaySeen {
        run: run.to_owned(),
        running,
        target_port,
        reaches,
    }
}

/// Acceptance (3): a relay that is down is started again towards the
/// broker's port; one that cannot be is the inbox's attention, told once,
/// and the worker set to the container waits with the relay named as why;
/// once every relay runs the attention ends. A relay that runs and does
/// not reach the broker has the machine started again first.
#[test]
fn the_supervisor_starts_a_relay_again_and_tells_the_inbox_when_it_cannot() {
    use crate::runtime_support::{
        TestWorkspace, VALID_AGENT, fixture, git, supervise_options, supervise_with,
    };
    let (_fixture, repo, db) = fixture();
    fs::write(
        repo.join("dagq.toml"),
        "[roles.worker]\nruntime = \"container\"\n",
    )
    .unwrap();
    git(&repo, &["add", "dagq.toml"]);
    git(&repo, &["commit", "-m", "runtimes"]);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let options = |relays: Arc<FakeRelays>| dagq::compose::SuperviseOptions {
        queue_broker: Some(dagq::compose::QueueBrokerOptions {
            executable: "/unused".into(),
            interval: Duration::ZERO,
            start_timeout: TIMEOUT,
            control: Some(dagq::compose::QueueBrokerControlPort(Arc::new(
                RunningBroker,
            ))),
        }),
        broker_relay: Some(dagq::compose::QueueRelayOptions {
            interval: Duration::ZERO,
            control: Some(dagq::compose::QueueRelayControlPort(relays)),
        }),
        ..supervise_options(1, true)
    };

    let failing = Arc::new(FakeRelays {
        seen: Mutex::new(vec![relay("r9", false, None, None)]),
        starts_fail: true,
        ..FakeRelays::default()
    });
    for _ in 0..2 {
        supervise_with(&db, &repo, &backend, &options(failing.clone())).unwrap();
    }
    assert_eq!(
        *failing.restarts.lock().unwrap(),
        [("r9".to_owned(), 4321), ("r9".to_owned(), 4321)]
    );
    let down = events(&db, "queue_relay_down");
    assert_eq!(down.len(), 1, "{down:?}");
    assert_eq!(down[0]["payload"]["reason"], "start_failed");
    let status = ok(&db, &["status"]);
    let attention = status["attention"].as_array().unwrap();
    let entry = attention
        .iter()
        .find(|entry| entry["kind"] == "queue_relay_down")
        .unwrap_or_else(|| panic!("{status}"));
    assert_eq!(entry["next"], "look at the dagq podman machine's relays");
    let container = attention
        .iter()
        .find(|entry| entry["kind"] == "container_runtime_unavailable")
        .unwrap_or_else(|| panic!("{status}"));
    assert!(
        container["last_error"]
            .as_str()
            .unwrap()
            .contains("relay to the queue broker could not be kept running"),
        "{container}"
    );
    assert!(backend.launched.lock().unwrap().is_empty());

    // A relay that starts: it runs towards the broker, and at the next look
    // the attention ends.
    let starting = Arc::new(FakeRelays {
        seen: Mutex::new(vec![relay("r9", true, Some(1111), Some(true))]),
        ..FakeRelays::default()
    });
    for _ in 0..2 {
        supervise_with(&db, &repo, &backend, &options(starting.clone())).unwrap();
    }
    assert_eq!(
        *starting.restarts.lock().unwrap(),
        [("r9".to_owned(), 4321)]
    );
    let started = events(&db, "queue_relay_started");
    assert_eq!(started.len(), 1, "{started:?}");
    assert_eq!(started[0]["payload"]["run"], "r9");
    assert_eq!(started[0]["payload"]["machine_restarted"], false);
    assert_eq!(events(&db, "queue_relay_running").len(), 1);
    assert!(
        !ok(&db, &["status"])["attention"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["kind"] == "queue_relay_down")
    );

    // A relay that runs and does not reach the broker: the machine is
    // started again, then every relay.
    let cut = Arc::new(FakeRelays {
        seen: Mutex::new(vec![
            relay("r9", true, Some(4321), Some(false)),
            relay("r8", true, Some(4321), Some(true)),
        ]),
        ..FakeRelays::default()
    });
    supervise_with(&db, &repo, &backend, &options(cut.clone())).unwrap();
    assert_eq!(cut.machine_restarts.load(Ordering::SeqCst), 1);
    assert_eq!(
        *cut.restarts.lock().unwrap(),
        [("r9".to_owned(), 4321), ("r8".to_owned(), 4321)]
    );
    let started = events(&db, "queue_relay_started");
    assert_eq!(started.len(), 3, "{started:?}");
    assert_eq!(started[2]["payload"]["machine_restarted"], true);
}

/// `dagq` in client mode from a directory that is no repository, with
/// `env` added.
fn client(dir: &Path, env: &[(&str, &str)], args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dagq"));
    command.without_actor_env().current_dir(dir).args(args);
    for name in [SOCKET_ENV, URL_ENV, CREDENTIAL_FILE_ENV] {
        command.env_remove(name);
    }
    command.envs(env.iter().copied());
    command.bounded_output().unwrap()
}

fn answered(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn refused(output: &Output) -> String {
    assert!(!output.status.success(), "it succeeded");
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    error["queue_service"]["code"]
        .as_str()
        .unwrap_or_else(|| panic!("no code in {error}"))
        .to_owned()
}

/// Stops the broker when it goes.
struct StopBroker(SystemQueueBroker);

impl Drop for StopBroker {
    fn drop(&mut self) {
        let _ = self.0.stop(TIMEOUT);
    }
}

/// Acceptance (7): on the host, without a VM: the forwarder on a port of
/// 127.0.0.1 → the run's unix socket → the relay (`dagq relay serve`, a
/// process of the test's) → the real broker → the real service. A
/// client-mode `dagq` given the broker's URL and its run's token shows and
/// notes as the run's worker; one with no token, a revoked one, or both a
/// socket and a URL is refused, read as the socket's client reads it.
#[test]
fn a_run_s_dagq_reaches_the_service_through_the_forwarder_the_relay_and_the_broker() {
    let _test = common::test();
    let queue = queue();
    let elsewhere = queue.dir().join("elsewhere");
    fs::create_dir(&elsewhere).unwrap();
    start(&queue);
    let control =
        SystemQueueBroker::new(&queue.db, &common::service::owned_executable(queue.dir()));
    let _stop = StopBroker(control.clone());
    let address = {
        let _waiting = common::within(TIMEOUT * 2, "the queue broker to start");
        control.start(TIMEOUT).unwrap().address.unwrap()
    };

    // The run's socket in a short directory: a socket's path is short.
    let sockets = tempfile::Builder::new()
        .prefix("dq")
        .tempdir_in("/tmp")
        .unwrap();
    let socket = sockets.path().join("r.sock");
    let relay = Command::new(env!("CARGO_BIN_EXE_dagq"))
        .without_actor_env()
        .args(["relay", "serve", "--socket"])
        .arg(&socket)
        .args(["--target", &address])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _relay = common::KillOnDrop::new(relay, "the relay");
    {
        let _waiting = common::within(TIMEOUT, "the relay's socket");
        while !socket.exists() {
            thread::sleep(Duration::from_millis(20));
        }
    }
    assert_eq!(
        fs::metadata(&socket).unwrap().permissions().mode() & 0o777,
        0o600
    );
    // The whole path answers the broker's health.
    assert_eq!(
        broker::health_through(&socket, TIMEOUT).unwrap()["build"],
        dagq::VERSION
    );

    // The forwarder, as `dagq relay forward` runs it in the container.
    assert_eq!(&forward_command()[..3], ["dagq", "relay", "forward"]);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let stop = Arc::new(AtomicBool::new(false));
    let forwarder = {
        let (stop, socket) = (stop.clone(), socket.clone());
        thread::spawn(move || forward_on(&listener, &socket, &stop, Duration::from_millis(10)))
    };

    let run = queue.run.id().to_string();
    let task = queue.run.task_id().to_string();
    let principal = Principal::worker(queue.run.id(), queue.run.task_id());
    let credential = service::issue(queue.dir(), &principal, 1).unwrap().file;
    let credential = credential.to_str().unwrap();
    let as_run = [
        (URL_ENV, url.as_str()),
        (CREDENTIAL_FILE_ENV, credential),
        ("DAGQ_RUN_ID", run.as_str()),
    ];
    let noted = answered(&client(
        &elsewhere,
        &as_run,
        &["note", "--task", &task, "--text", "through the relay"],
    ));
    assert_eq!(noted["payload"]["by"], "worker", "{noted}");
    let shown = answered(&client(&elsewhere, &as_run, &["show", &task]));
    assert_eq!(shown, ok(&queue.db, &["show", &task]));
    assert!(shown.to_string().contains("through the relay"), "{shown}");
    let located = answered(&client(&elsewhere, &as_run, &["locate"]));
    assert_eq!(located["broker"], json!(url.trim_start_matches("http://")));
    assert_eq!(located["db"], Value::Null);

    // Refused by the broker, read as the socket's client reads a refusal.
    let without_token = [(URL_ENV, url.as_str()), ("DAGQ_RUN_ID", run.as_str())];
    assert_eq!(
        refused(&client(&elsewhere, &without_token, &["show", &task])),
        "unauthenticated"
    );
    // A token the control side revoked (its file kept by the run).
    let kept = sockets.path().join("kept-token");
    fs::copy(credential, &kept).unwrap();
    assert!(service::revoke(queue.dir(), &principal.actor_id).unwrap());
    let revoked = [
        (URL_ENV, url.as_str()),
        (CREDENTIAL_FILE_ENV, kept.to_str().unwrap()),
        ("DAGQ_RUN_ID", run.as_str()),
    ];
    assert_eq!(
        refused(&client(&elsewhere, &revoked, &["show", &task])),
        "unauthenticated"
    );
    let denied: Vec<Value> = events(&queue.db, "queue_broker_denied")
        .into_iter()
        .map(|event| event["payload"].clone())
        .collect();
    let reasons: Vec<&str> = denied
        .iter()
        .map(|payload| payload["reason"].as_str().unwrap())
        .collect();
    assert_eq!(reasons, ["missing_token", "unknown_token"], "{denied:?}");
    // Both a socket and a URL: refused before anything is sent.
    let socket_too = service::socket_path(queue.dir());
    let both = [
        (URL_ENV, url.as_str()),
        (SOCKET_ENV, socket_too.to_str().unwrap()),
        (CREDENTIAL_FILE_ENV, credential),
    ];
    assert_eq!(
        refused(&client(&elsewhere, &both, &["show", &task])),
        "client_misconfigured"
    );

    stop.store(true, Ordering::SeqCst);
    assert!(forwarder.join().unwrap() >= 4);
}

/// The real dagq machine: a relay in it reaches the host's broker through
/// gvproxy. It needs podman, the dagq machine and an image with the Linux
/// dagq client (`DAGQ_RELAY_IMAGE`), so it is not counted in the gate; a
/// person or the inbox runs it.
#[test]
#[ignore = "needs podman, the dagq machine and DAGQ_RELAY_IMAGE"]
fn a_relay_in_the_real_machine_reaches_the_host_s_broker() {
    let _waiting = common::within(Duration::from_secs(1800), "the real relay");
    let image = std::env::var("DAGQ_RELAY_IMAGE").expect("DAGQ_RELAY_IMAGE");
    let queue = queue();
    start(&queue);
    let control =
        SystemQueueBroker::new(&queue.db, &common::service::owned_executable(queue.dir()));
    let _stop = StopBroker(control.clone());
    let address = control.start(TIMEOUT).unwrap().address.unwrap();
    let port = dagq::domain::broker_relay::address_port(&address).unwrap();
    // The queue's directory is not a data home's, so the relay is named
    // after a key of the test's.
    let data = tempfile::tempdir().unwrap();
    let queue_dir = data.path().join("dagq").join("relaytest0000000");
    fs::create_dir_all(&queue_dir).unwrap();
    let db = queue_dir.join("queue.db");
    fs::write(&db, "").unwrap();
    let relays = PodmanRelays::of_queue(PodmanCli::default(), &db, None);
    let spec = RelaySpec {
        run: "realrun".to_owned(),
        image,
        uid: dagq::domain::broker_relay::CONTAINER_UID,
    };
    relays.open(&spec, port).unwrap();
    let seen = relays.relays().unwrap();
    relays.close("realrun").unwrap();
    assert_eq!(
        seen,
        [RelaySeen {
            run: "realrun".to_owned(),
            running: true,
            target_port: Some(port),
            reaches: Some(true),
        }]
    );
}
