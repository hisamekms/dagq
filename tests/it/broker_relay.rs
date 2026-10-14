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

use dagq::application::ContainerRuntime;
use dagq::application::broker_relay::{RelayControl, RelayGate};
use dagq::application::queue_broker::{BrokerProbe, QueueBrokerControl};
use dagq::domain::broker_relay::{RelaySeen, RelaySpec, RelayState, forward_command};
use dagq::domain::container_runtime::{Availability, RuntimeTarget};
use dagq::domain::queue_service::{
    CREDENTIAL_FILE_ENV, Principal, SOCKET_ENV, ServiceState, URL_ENV,
};
use dagq::infrastructure::broker_relay::{PodmanRelays, QueueRelayHealth, forward_on};
use dagq::infrastructure::podman_machine::PodmanCli;
use dagq::infrastructure::queue_broker::{self as broker, SystemQueueBroker};
use dagq::infrastructure::queue_service as service;
use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(10);

/// A `podman` that keeps the containers and volumes of the dagq machine
/// in `state`, writes each command line to `state/calls`, fails a relay's
/// `run` while `state/fail-run` is there, answers a relay's probe as
/// unreachable while `state/unreachable` is there, fails every
/// `--connection dagq` command as a connection through a gvproxy that is
/// down does while `state/no-connection` is there (a machine's start
/// removes it unless `state/start-fails` is there), and hangs every
/// `--connection dagq` command while `state/hang` is there until it is
/// killed or its parent ends (a machine's start mends that too), a
/// `--connection dagq <verb>` while `state/hang-<verb>` is there and a
/// `machine <verb>` while `state/hang-machine-<verb>` is, each hung one
/// writing its pid to `state/hung`. A machine's stop takes its containers
/// with it.
fn fake_podman(state: &Path) -> PathBuf {
    fs::create_dir_all(state.join("containers")).unwrap();
    fs::create_dir_all(state.join("volumes")).unwrap();
    let s = shell_path(state);
    let script = format!(
        r#"#!/bin/sh
S={s}
printf '%s\n' "$*" >> "$S/calls"
hang() {{
  echo $$ >> "$S/hung"
  # Hangs until killed, and ends when the test that ran it goes.
  while kill -0 "$PPID" 2>/dev/null; do sleep 0.1; done
  exit 125
}}
[ "$1" = machine ] && [ -f "$S/hang-machine-$2" ] && hang
[ "$1 $2" = "--connection dagq" ] && [ -f "$S/hang-$3" ] && hang
case "$1 $2" in
"machine list") printf '[{{"Name":"dagq","Running":true}}]' ;;
"machine stop") rm -f "$S"/containers/* ;;
"machine start")
  [ -f "$S/start-fails" ] && {{ echo "cannot start" >&2; exit 125; }}
  rm -f "$S/no-connection" "$S/hang" ;;
"system connection") printf '[{{"Name":"dagq","Default":true}}]' ;;
"--connection dagq")
  [ -f "$S/hang" ] && hang
  [ -f "$S/no-connection" ] && {{ echo "Cannot connect to Podman" >&2; exit 125; }}
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
    if [ "$2" = exists ]; then [ -f "$S/containers/$name" ] || exit 1; exit 0; fi
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
            call.contains(&format!("--volume {}:/run/dagq-broker:z,U ", volume(run))),
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
            state: RelayState::Running,
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
    assert_eq!(relays.relays().unwrap()[0].state, RelayState::Stopped);
    relays.restart("r2", 5555).unwrap();
    assert_eq!(relays.relays().unwrap()[0].target_port, Some(5555));
    assert!(relays.open(&spec("../x"), 1).is_err());
}

/// The relays of a test's hands: what each look finds, whether a start
/// or a close works, with the starts of relays and of the machine and the
/// closes counted. A relay closed is not found again.
#[derive(Default)]
struct FakeRelays {
    seen: Mutex<Vec<RelaySeen>>,
    starts_fail: bool,
    closes_fail: AtomicBool,
    restarts: Mutex<Vec<(String, u16)>>,
    closes: Mutex<Vec<String>>,
    machine_restarts: AtomicU32,
}

impl RelayControl for FakeRelays {
    fn open(&self, _: &RelaySpec, _: u16) -> anyhow::Result<()> {
        unreachable!("the supervisor opens no relay")
    }
    fn close(&self, run: &str) -> anyhow::Result<()> {
        self.closes.lock().unwrap().push(run.to_owned());
        if self.closes_fail.load(Ordering::SeqCst) {
            anyhow::bail!("podman cannot remove the relay");
        }
        self.seen.lock().unwrap().retain(|relay| relay.run != run);
        Ok(())
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
                relay.state = RelayState::Running;
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
        state: if running {
            RelayState::Running
        } else {
            RelayState::Stopped
        },
        target_port,
        reaches,
    }
}

/// A run of a new ready task claimed outside the supervisor, left in
/// `status` with no lease and its task in `task`: a `needs_session` run of
/// a task `in_progress` lives (nothing resumes it while the container
/// runtime cannot be used); a run of a task `completed` or `canceled` is
/// over.
fn run_in(db: &Path, repo: &Path, status: &str, task: &str) -> String {
    use crate::runtime_support::{SqliteQueue, add_ready_task, git_out};
    use dagq::domain::{ClaimOutcome, CommitSha, LeaseToken};
    let mut queue = SqliteQueue::open(db).unwrap();
    add_ready_task(&mut queue, &format!("a run {status}"), &[]);
    let base = CommitSha::try_from(git_out(repo, &["rev-parse", "main"]).as_str()).unwrap();
    let ClaimOutcome::Claimed { run } = queue
        .claim_for_supervisor(&base, &LeaseToken::new("gone"))
        .unwrap()
    else {
        panic!("nothing was claimed")
    };
    let raw = rusqlite::Connection::open(db).unwrap();
    raw.execute(
        "UPDATE task_runs SET status=?2 WHERE id=?1",
        rusqlite::params![run.id().as_str(), status],
    )
    .unwrap();
    raw.execute(
        "UPDATE tasks SET status=?2 WHERE id=(SELECT task_id FROM task_runs WHERE id=?1)",
        rusqlite::params![run.id().as_str(), task],
    )
    .unwrap();
    raw.execute(
        "DELETE FROM run_leases WHERE run_id=?1",
        [run.id().as_str()],
    )
    .unwrap();
    run.id().to_string()
}

/// The fixture's repository set to run its workers in containers, and the
/// supervisor's options with `relays` and a running broker.
fn container_fixture() -> (
    crate::runtime_support::Fixture,
    PathBuf,
    PathBuf,
    crate::runtime_support::TestWorkspace,
) {
    use crate::runtime_support::{TestWorkspace, VALID_AGENT, fixture, git};
    let (fixture, repo, db) = fixture();
    fs::write(
        repo.join("dagq.toml"),
        "[roles.worker]\nruntime = \"container\"\n",
    )
    .unwrap();
    git(&repo, &["add", "dagq.toml"]);
    git(&repo, &["commit", "-m", "runtimes"]);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    (fixture, repo, db, backend)
}

fn relay_options(relays: Arc<dyn RelayControl>) -> dagq::compose::SuperviseOptions {
    dagq::compose::SuperviseOptions {
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
        ..crate::runtime_support::supervise_options(1, true)
    }
}

/// Acceptance (3): a relay that is down is started again towards the
/// broker's port; one that cannot be is the inbox's attention, told once,
/// and the worker set to the container waits with the relay named as why;
/// once every relay runs the attention ends. A relay that runs and does
/// not reach the broker has the machine started again first. The relays
/// are of runs that live.
#[test]
fn the_supervisor_starts_a_relay_again_and_tells_the_inbox_when_it_cannot() {
    use crate::runtime_support::supervise_with;
    let (_fixture, repo, db, backend) = container_fixture();
    let options = relay_options;
    let r9 = run_in(&db, &repo, "needs_session", "in_progress");
    let r8 = run_in(&db, &repo, "needs_session", "in_progress");
    let (r9, r8) = (r9.as_str(), r8.as_str());

    let failing = Arc::new(FakeRelays {
        seen: Mutex::new(vec![relay(r9, false, None, None)]),
        starts_fail: true,
        ..FakeRelays::default()
    });
    for _ in 0..2 {
        supervise_with(&db, &repo, &backend, &options(failing.clone())).unwrap();
    }
    assert_eq!(
        *failing.restarts.lock().unwrap(),
        [(r9.to_owned(), 4321), (r9.to_owned(), 4321)]
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
        seen: Mutex::new(vec![relay(r9, true, Some(1111), Some(true))]),
        ..FakeRelays::default()
    });
    for _ in 0..2 {
        supervise_with(&db, &repo, &backend, &options(starting.clone())).unwrap();
    }
    assert_eq!(*starting.restarts.lock().unwrap(), [(r9.to_owned(), 4321)]);
    let started = events(&db, "queue_relay_started");
    assert_eq!(started.len(), 1, "{started:?}");
    assert_eq!(started[0]["payload"]["run"], r9);
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
            relay(r9, true, Some(4321), Some(false)),
            relay(r8, true, Some(4321), Some(true)),
        ]),
        ..FakeRelays::default()
    });
    supervise_with(&db, &repo, &backend, &options(cut.clone())).unwrap();
    assert_eq!(cut.machine_restarts.load(Ordering::SeqCst), 1);
    assert_eq!(
        *cut.restarts.lock().unwrap(),
        [(r9.to_owned(), 4321), (r8.to_owned(), 4321)]
    );
    let started = events(&db, "queue_relay_started");
    assert_eq!(started.len(), 3, "{started:?}");
    assert_eq!(started[2]["payload"]["machine_restarted"], true);
}

/// Acceptance (2): the relays a run that is over left recorded (its task
/// is over, or the queue has no such run), with their image gone so a start
/// would fail, are closed and not started again: they make no
/// `queue_relay_started` or `queue_relay_down` and count for no start, an
/// attention left standing ends, and the container runtime is not held
/// for the relays. A close that fails is tried again at the next look,
/// while a living run's relay is still started again.
#[test]
fn the_supervisor_closes_the_relays_of_runs_that_are_over() {
    use crate::runtime_support::supervise_with;
    let (_fixture, repo, db, backend) = container_fixture();
    let failed = run_in(&db, &repo, "failed", "canceled");
    let succeeded = run_in(&db, &repo, "succeeded", "completed");
    // A run that did not end whose task is over.
    let canceled = run_in(&db, &repo, "needs_session", "canceled");
    let over = [
        failed.as_str(),
        succeeded.as_str(),
        canceled.as_str(),
        "gone-run",
    ];
    // An attention left by a supervisor that started them again before.
    crate::runtime_support::SqliteQueue::open(&db)
        .unwrap()
        .record_queue_event(
            dagq::domain::event_kind::EventKind::QueueRelayDown,
            json!({"reason": "start_failed", "message": "the relay of run gone-run"}),
        )
        .unwrap();
    let relays = Arc::new(FakeRelays {
        seen: Mutex::new(
            over.iter()
                .map(|run| relay(run, false, None, None))
                .collect(),
        ),
        starts_fail: true,
        ..FakeRelays::default()
    });
    relays.closes_fail.store(true, Ordering::SeqCst);
    supervise_with(&db, &repo, &backend, &relay_options(relays.clone())).unwrap();
    // The close failed: each look tries it again, and the records stay.
    let tried = relays.closes.lock().unwrap().clone();
    assert!(
        !tried.is_empty() && tried.len().is_multiple_of(4),
        "{tried:?}"
    );
    assert!(tried.chunks(4).all(|look| look == over), "{tried:?}");
    assert_eq!(relays.seen.lock().unwrap().len(), 4);
    relays.closes_fail.store(false, Ordering::SeqCst);
    supervise_with(&db, &repo, &backend, &relay_options(relays.clone())).unwrap();
    assert_eq!(relays.closes.lock().unwrap()[tried.len()..], over);
    assert!(relays.seen.lock().unwrap().is_empty());
    // Closed, they are not looked at again.
    supervise_with(&db, &repo, &backend, &relay_options(relays.clone())).unwrap();
    assert_eq!(relays.closes.lock().unwrap().len(), tried.len() + 4);

    assert!(relays.restarts.lock().unwrap().is_empty());
    assert_eq!(relays.machine_restarts.load(Ordering::SeqCst), 0);
    assert!(events(&db, "queue_relay_started").is_empty());
    assert_eq!(events(&db, "queue_relay_down").len(), 1);
    assert_eq!(events(&db, "queue_relay_running").len(), 1);
    let status = ok(&db, &["status"]);
    let attention = status["attention"].as_array().unwrap();
    assert!(
        !attention
            .iter()
            .any(|entry| entry["kind"] == "queue_relay_down"),
        "{status}"
    );
    assert!(
        !attention.iter().any(|entry| entry["last_error"]
            .as_str()
            .is_some_and(|error| error.contains("relay to the queue broker"))),
        "{status}"
    );

    // A living run's relay beside one that is over: the one is closed, the
    // other started again, and only its start is counted.
    let living = run_in(&db, &repo, "needs_session", "in_progress");
    let mixed = Arc::new(FakeRelays {
        seen: Mutex::new(vec![
            relay(&failed, false, None, None),
            relay(&living, false, None, None),
        ]),
        ..FakeRelays::default()
    });
    supervise_with(&db, &repo, &backend, &relay_options(mixed.clone())).unwrap();
    assert_eq!(*mixed.closes.lock().unwrap(), [failed]);
    assert_eq!(*mixed.restarts.lock().unwrap(), [(living.clone(), 4321)]);
    let started = events(&db, "queue_relay_started");
    assert_eq!(started.len(), 1, "{started:?}");
    assert_eq!(started[0]["payload"]["run"], living.as_str());
}

/// The relays of a queue of the test's in `dir` through the fake podman
/// of `state`, each look's command given `look_timeout`.
fn podman_relays(dir: &Path, state: &Path, look_timeout: Duration) -> PodmanRelays {
    let queue_dir = dir.join("data").join("dagq").join(QUEUE);
    fs::create_dir_all(&queue_dir).unwrap();
    let db = queue_dir.join("queue.db");
    fs::write(&db, "").unwrap();
    PodmanRelays {
        look_timeout,
        ..PodmanRelays::of_queue(
            PodmanCli {
                program: fake_podman(state),
            },
            &db,
            None,
        )
    }
}

/// The index of the first call of `state`'s podman that starts with
/// `prefix`.
fn first_call(state: &Path, prefix: &str) -> usize {
    calls(state)
        .iter()
        .position(|call| call.starts_with(prefix))
        .unwrap_or_else(|| panic!("no {prefix}: {:?}", calls(state)))
}

/// When podman cannot reach the machine (its
/// gvproxy is down), a look reads the relays as unreachable, not as not
/// running, and the supervisor starts the machine again before any relay;
/// when the machine cannot be started either, the inbox's attention stands
/// and the container runtime cannot be used.
#[test]
fn podman_that_cannot_reach_the_machine_has_the_machine_started_again_before_the_relays() {
    use crate::runtime_support::supervise_with;
    let (_fixture, repo, db, backend) = container_fixture();
    // Runs that live: the watch closes the relays of runs that are over.
    let r1 = run_in(&db, &repo, "needs_session", "in_progress");
    let r2 = run_in(&db, &repo, "needs_session", "in_progress");
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("podman");
    let relays = Arc::new(podman_relays(dir.path(), &state, TIMEOUT));
    relays.open(&spec(&r1), 4321).unwrap();
    relays.open(&spec(&r2), 4321).unwrap();
    fs::write(state.join("no-connection"), "").unwrap();
    assert_eq!(
        relays
            .relays()
            .unwrap()
            .iter()
            .map(|relay| relay.state)
            .collect::<Vec<_>>(),
        [RelayState::PodmanUnreachable; 2]
    );

    let options = relay_options(relays.clone());
    let before = calls(&state).len();
    supervise_with(&db, &repo, &backend, &options).unwrap();
    let start = first_call(&state, "machine start dagq");
    assert!(start >= before, "{:?}", calls(&state));
    let relay_runs: Vec<usize> = calls(&state)
        .iter()
        .enumerate()
        .skip(before)
        .filter(|(_, call)| call.starts_with("--connection dagq run "))
        .map(|(index, _)| index)
        .collect();
    assert_eq!(relay_runs.len(), 2, "{:?}", calls(&state));
    assert!(relay_runs.iter().all(|index| *index > start));
    let started = events(&db, "queue_relay_started");
    assert_eq!(started.len(), 2, "{started:?}");
    assert!(
        started
            .iter()
            .all(|event| event["payload"]["machine_restarted"] == true),
        "{started:?}"
    );
    assert!(events(&db, "queue_relay_down").is_empty());

    // The machine cannot be started again either: the attention stands
    // and the worker set to the container waits.
    fs::write(state.join("no-connection"), "").unwrap();
    fs::write(state.join("start-fails"), "").unwrap();
    supervise_with(&db, &repo, &backend, &options).unwrap();
    let down = events(&db, "queue_relay_down");
    assert_eq!(down.len(), 1, "{down:?}");
    assert_eq!(down[0]["payload"]["reason"], "start_failed");
    let gate = RelayGate {
        records: Arc::new(QueueRelayHealth { db: db.clone() }),
        container: Usable,
    };
    match gate.availability(RuntimeTarget::Worker) {
        Availability::Unavailable(why) => assert!(
            why.contains("relay to the queue broker could not be kept running"),
            "{why}"
        ),
        Availability::Available => panic!("the container runtime is usable"),
    }
    assert!(backend.launched.lock().unwrap().is_empty());
    assert_eq!(events(&db, "queue_relay_started").len(), 2);
}

/// A container runtime that is usable but for the relays' gate.
struct Usable;

impl ContainerRuntime for Usable {
    fn availability(&self, _: RuntimeTarget) -> Availability {
        Availability::Available
    }
}

/// A podman of a look that hangs is killed
/// at the look's limit and read as not reaching the machine, the rest of
/// the relays are not asked, and the supervisor's pass goes on to start
/// the machine and the relays again.
#[test]
fn a_hung_podman_of_a_look_is_killed_and_the_supervisor_goes_on() {
    use crate::runtime_support::supervise_with;
    let (_fixture, repo, db, backend) = container_fixture();
    // Runs that live: the watch closes the relays of runs that are over.
    let r1 = run_in(&db, &repo, "needs_session", "in_progress");
    let r2 = run_in(&db, &repo, "needs_session", "in_progress");
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("podman");
    // Long enough for the fake's shell to note itself under load.
    let limit = Duration::from_secs(1);
    let relays = Arc::new(podman_relays(dir.path(), &state, limit));
    relays.open(&spec(&r1), 4321).unwrap();
    relays.open(&spec(&r2), 4321).unwrap();
    fs::write(state.join("hang"), "").unwrap();
    let before = calls(&state).len();
    let looked = std::time::Instant::now();
    let seen = relays.relays().unwrap();
    assert!(looked.elapsed() < TIMEOUT, "{:?}", looked.elapsed());
    assert_eq!(
        seen.iter().map(|relay| relay.state).collect::<Vec<_>>(),
        [RelayState::PodmanUnreachable; 2]
    );
    // One hung command, killed: the second relay was not asked.
    assert_eq!(calls(&state).len(), before + 1, "{:?}", calls(&state));
    let hung = fs::read_to_string(state.join("hung")).unwrap();
    assert_eq!(hung.lines().count(), 1, "{hung}");
    let alive = Command::new("kill")
        .args(["-0", hung.trim()])
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(!alive.success(), "the hung podman {hung} still runs");

    let options = relay_options(relays.clone());
    let passed = std::time::Instant::now();
    supervise_with(&db, &repo, &backend, &options).unwrap();
    assert!(passed.elapsed() < TIMEOUT * 3, "{:?}", passed.elapsed());
    let started = events(&db, "queue_relay_started");
    assert_eq!(started.len(), 2, "{started:?}");
    assert_eq!(started[0]["payload"]["machine_restarted"], true);
    assert_eq!(
        relays
            .relays()
            .unwrap()
            .iter()
            .map(|relay| relay.state)
            .collect::<Vec<_>>(),
        [RelayState::Running; 2]
    );
}

/// Every podman of `state` that hung has been killed: none still runs.
fn hung_ones_killed(state: &Path) {
    // A fake killed before it noted itself left no pid.
    let hung = fs::read_to_string(state.join("hung")).unwrap_or_default();
    for pid in hung.lines() {
        let alive = Command::new("kill")
            .args(["-0", pid])
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(!alive.success(), "the hung podman {pid} still runs");
    }
}

/// `act` fails within the test's timeout, as a podman command that did
/// not end in time.
fn fails_in_time(what: &str, act: impl FnOnce() -> anyhow::Result<()>) {
    let began = std::time::Instant::now();
    let error = act().expect_err(what);
    assert!(began.elapsed() < TIMEOUT, "{what}: {:?}", began.elapsed());
    assert!(
        format!("{error:#}").contains("did not end within"),
        "{what}: {error:#}"
    );
}

/// The limit of a test's podman command that hangs: long enough for the
/// fake's shell to note itself under load, short so the test is.
const HUNG_LIMIT: Duration = Duration::from_millis(700);

/// Hang each of `verbs` of `state`'s podman in turn (`hang-<verb>`), and
/// have `act` fail in time on each.
fn each_hung(state: &Path, verbs: &[&str], act: impl Fn() -> anyhow::Result<()>) {
    for verb in verbs {
        let hang = state.join(format!("hang-{verb}"));
        fs::write(&hang, "").unwrap();
        fails_in_time(verb, &act);
        fs::remove_file(&hang).unwrap();
    }
    hung_ones_killed(state);
}

/// A podman of a relay's start again (`rm`, `volume create`, `run`) that
/// hangs is killed at the relay's limit and the start fails, which the
/// supervisor tells the inbox as `start_failed`.
#[test]
fn a_hung_podman_of_a_relay_s_start_is_killed_and_the_start_fails() {
    let _test = common::test();
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("podman");
    let relays = podman_relays(dir.path(), &state, TIMEOUT);
    relays.open(&spec("r1"), 4321).unwrap();
    let limited = PodmanRelays {
        relay_timeout: HUNG_LIMIT,
        ..relays.clone()
    };
    each_hung(&state, &["rm", "volume", "run"], || {
        limited.restart("r1", 4321)
    });
    relays.restart("r1", 4321).unwrap();
}

/// A podman of the close of a relay (`rm`, `volume exists`) that hangs is
/// killed at the relay's limit and the close fails, which the supervisor
/// tries again at the next look; the record stays until a close succeeds.
#[test]
fn a_hung_podman_of_a_relay_s_close_is_killed_and_the_close_fails() {
    let _test = common::test();
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("podman");
    let relays = podman_relays(dir.path(), &state, TIMEOUT);
    relays.open(&spec("r1"), 4321).unwrap();
    let limited = PodmanRelays {
        relay_timeout: HUNG_LIMIT,
        ..relays.clone()
    };
    each_hung(&state, &["rm", "volume"], || limited.close("r1"));
    assert_eq!(relays.relays().unwrap().len(), 1);
    relays.close("r1").unwrap();
    assert!(relays.relays().unwrap().is_empty());
}

/// A podman of the machine's start again from the watch (`machine list`,
/// `stop`, `start`) that hangs is killed at the machine's limit, which is
/// not the relay's, and the start again fails.
#[test]
fn a_hung_podman_of_the_machine_s_start_again_is_killed_and_it_fails() {
    let _test = common::test();
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("podman");
    let relays = PodmanRelays {
        machine_timeout: HUNG_LIMIT,
        ..podman_relays(dir.path(), &state, TIMEOUT)
    };
    assert_eq!(
        relays.relay_timeout,
        dagq::infrastructure::broker_relay::RELAY_TIMEOUT
    );
    assert!(
        dagq::infrastructure::podman_machine::MACHINE_TIMEOUT
            > dagq::infrastructure::broker_relay::RELAY_TIMEOUT
    );
    each_hung(
        &state,
        &["machine-list", "machine-stop", "machine-start"],
        || relays.restart_machine(),
    );
    relays.restart_machine().unwrap();
}

/// When podman does not reach the machine and the machine's start again
/// hangs, it is killed and the supervisor's pass goes on and tells the
/// inbox with `queue_relay_down`, while nothing set to the container runs.
#[test]
fn a_hung_machine_start_again_is_the_inbox_s_attention() {
    use crate::runtime_support::supervise_with;
    let (_fixture, repo, db, backend) = container_fixture();
    let living = run_in(&db, &repo, "needs_session", "in_progress");
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("podman");
    let relays = Arc::new(PodmanRelays {
        machine_timeout: Duration::from_secs(1),
        ..podman_relays(dir.path(), &state, TIMEOUT)
    });
    relays.open(&spec(&living), 4321).unwrap();
    fs::write(state.join("no-connection"), "").unwrap();
    fs::write(state.join("hang-machine-start"), "").unwrap();
    let passed = std::time::Instant::now();
    supervise_with(&db, &repo, &backend, &relay_options(relays)).unwrap();
    assert!(passed.elapsed() < TIMEOUT * 3, "{:?}", passed.elapsed());
    hung_ones_killed(&state);
    let down = events(&db, "queue_relay_down");
    assert_eq!(down.len(), 1, "{down:?}");
    assert_eq!(down[0]["payload"]["reason"], "start_failed");
    let message = down[0]["payload"]["message"].as_str().unwrap();
    assert!(
        message.contains("start the dagq machine again") && message.contains("did not end within"),
        "{message}"
    );
    assert!(events(&db, "queue_relay_started").is_empty());
    let status = ok(&db, &["status"]);
    assert!(
        status["attention"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["kind"] == "queue_relay_down"),
        "{status}"
    );
    assert!(backend.launched.lock().unwrap().is_empty());
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
            state: RelayState::Running,
            target_port: Some(port),
            reaches: Some(true),
        }]
    );
}
