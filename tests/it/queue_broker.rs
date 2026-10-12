//! The queue broker on the host (docs/design/queue-broker.md, ADR-t2114-1,
//! ADR-t1233-4): the `dagq` binary's broker takes the queue service's
//! requests on a TCP port of 127.0.0.1 in the shape
//! `dagq::domain::queue_broker` gives, passes a run's own token's on to the
//! service's unix socket and its answer back untouched, and refuses and
//! records every other request; `up` starts it and `down` stops it; a
//! supervisor starts it again, replaces one of another build once, stops it
//! at the end of the drain `down` asked for, and tells the inbox when it
//! cannot keep it, while nothing set to the container runs.

use crate::common;
use crate::runtime_support::service::{Running, idle_queue, supervisor_token};
use common::service::{events, queue, start, token};

use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::cli::ok;
use common::lifecycle::{FakeLaunchd, FakeProcesses};
use dagq::application::EventStore;
use dagq::application::queue_broker::{BrokerProbe, QueueBrokerControl};
use dagq::domain::queue_broker::{BEARER, HEALTH_PATH, RUN_HEADER};
use dagq::domain::queue_service::{
    API_VERSION, Principal, ServiceErrorCode, ServiceRequest, ServiceResponse, ServiceState,
    UseCase,
};
use dagq::domain::{RunId, TaskId};
use dagq::infrastructure::queue_broker::{self as broker, SystemQueueBroker};
use dagq::infrastructure::queue_service as service;
use dagq::infrastructure::sqlite::SqliteQueue;
use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(10);

/// Stops the queue's broker when it goes.
struct Stop(SystemQueueBroker);

impl Drop for Stop {
    fn drop(&mut self) {
        let _ = self.0.stop(TIMEOUT);
    }
}

fn request(use_case: UseCase, params: Value) -> ServiceRequest {
    ServiceRequest {
        api_version: API_VERSION,
        token: None,
        use_case,
        params,
    }
}

/// The payloads of the queue's `queue_broker_denied`, read on the DB.
fn denied(db: &Path) -> Vec<Value> {
    rusqlite::Connection::open(db)
        .unwrap()
        .prepare("SELECT payload FROM run_events WHERE kind = 'queue_broker_denied' ORDER BY id")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .map(|payload| serde_json::from_str(&payload.unwrap()).unwrap())
        .collect()
}

/// The build the broker of the binary before an `install` names.
const OLD_BUILD: &str = "0.0.1-old";

/// Only when [`broker_of_the_binary_before`] runs it: a process that runs
/// the broker of the queue at `DAGQ_OLD_BROKER_DB` as the binary before an
/// `install` or an automatic update would, naming [`OLD_BUILD`], until it
/// is stopped (SIGTERM ends it) or the test that started it is gone.
#[test]
#[ignore = "run by broker_of_the_binary_before"]
fn old_broker_probe() {
    let Some(db) = std::env::var_os("DAGQ_OLD_BROKER_DB") else {
        return;
    };
    broker::serve(&broker::BrokerServeOptions {
        db: db.into(),
        generators: dagq::infrastructure::clock::system(),
        stop: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        poll: Duration::from_millis(50),
        owner: service::owner_from_env(|name| std::env::var(name).ok()),
        build: OLD_BUILD.to_owned(),
    })
    .unwrap();
}

/// The broker of the queue at `db` as the binary before left it: a real
/// process answering with [`OLD_BUILD`], killed when the guard goes.
fn broker_of_the_binary_before(db: &Path) -> common::KillOnDrop {
    use common::service::OwnedByTest;
    let probe = common::KillOnDrop::new(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "queue_broker::old_broker_probe", "--ignored"])
            .env("DAGQ_OLD_BROKER_DB", db)
            .owned_by_test()
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
        "the broker of the binary before",
    );
    let _waiting = common::within(TIMEOUT, "the broker of the binary before to answer");
    while broker::probe(db.parent().unwrap()).build.as_deref() != Some(OLD_BUILD)
        || broker::probe(db.parent().unwrap()).state != ServiceState::Running
    {
        std::thread::sleep(Duration::from_millis(20));
    }
    probe
}

/// Wait for `probe`'s process to end: a broker replaced is stopped.
fn ended(mut probe: common::KillOnDrop) {
    let _waiting = common::within(TIMEOUT, "the broker of the binary before to end");
    probe.child().wait().unwrap();
}

fn response(body: &[u8]) -> ServiceResponse {
    serde_json::from_slice(body).unwrap_or_else(|error| {
        panic!("{error}: {}", String::from_utf8_lossy(body));
    })
}

/// A request for the service with a run's own token reaches it as the
/// run's principal and comes back as the service wrote it; every other
/// token, and every destination but the service, is refused with the code
/// a client reads and recorded with its run and destination.
#[test]
fn the_broker_passes_a_run_s_own_requests_to_the_service_and_refuses_the_rest() {
    let _test = common::test();
    let queue = queue();
    start(&queue);
    let control =
        SystemQueueBroker::new(&queue.db, &common::service::owned_executable(queue.dir()));
    let _stop = Stop(control.clone());
    let started = {
        let _waiting = common::within(TIMEOUT * 2, "the queue broker to start");
        control.start(TIMEOUT).unwrap()
    };
    assert!(started.current(), "{started:?}");
    let address = started.address.clone().unwrap();
    assert!(address.starts_with("127.0.0.1:"), "{address}");
    let run = queue.run.id().clone();
    let task = queue.run.task_id();
    let principal = Principal::worker(&run, task);
    let own = token(&queue, &principal);

    // Passed on with the run's token, the answer untouched: the same as the
    // service's own answer to the same request.
    let show = request(UseCase::Show, json!({"id": task.as_i64()}));
    let (status, body) =
        broker::call(&address, Some(&own), Some(run.as_str()), &show, TIMEOUT).unwrap();
    assert_eq!(status, 200);
    let direct = service::call(
        &service::socket_path(queue.dir()),
        &ServiceRequest {
            token: Some(own.clone()),
            ..show.clone()
        },
        TIMEOUT,
    )
    .unwrap();
    assert!(direct.ok, "{direct:?}");
    assert_eq!(response(&body), direct);
    // The service acts as the run's principal: the note is the worker's.
    let note = request(
        UseCase::Note,
        json!({"task": task.as_i64(), "text": "through the broker"}),
    );
    let (status, body) =
        broker::call(&address, Some(&own), Some(run.as_str()), &note, TIMEOUT).unwrap();
    assert_eq!(status, 200);
    let noted = response(&body);
    assert!(noted.ok, "{noted:?}");
    assert!(
        noted
            .result
            .unwrap()
            .to_string()
            .contains(&principal.actor_id),
        "the note is not the worker's"
    );

    // Refused with the service's shape of a refusal.
    let job = token(
        &queue,
        &Principal::of(&dagq::domain::ActorContext::review_job(&run, 1)),
    );
    let ghost = token(
        &queue,
        &Principal::worker(&RunId::new("ghost-run").unwrap(), TaskId::new(99)),
    );
    let gone = Principal::worker(&RunId::new("revoked-run").unwrap(), TaskId::new(98));
    let revoked = token(&queue, &gone);
    assert!(service::revoke(queue.dir(), &gone.actor_id).unwrap());
    for (bearer, named, reason) in [
        (None, Some(run.as_str()), "missing_token"),
        (Some(own.as_str()), None, "missing_run"),
        (Some(revoked.as_str()), Some("revoked-run"), "unknown_token"),
        (Some(job.as_str()), Some(run.as_str()), "not_a_run_token"),
        (Some(own.as_str()), Some("another-run"), "other_run"),
        (Some(ghost.as_str()), Some("ghost-run"), "run_ended"),
    ] {
        let (status, body) = broker::call(&address, bearer, named, &show, TIMEOUT).unwrap();
        assert_eq!(status, 401, "{reason}");
        let refused = response(&body);
        assert!(!refused.ok);
        assert_eq!(
            refused.error.unwrap().code,
            ServiceErrorCode::Unauthenticated,
            "{reason}"
        );
        let records = denied(&queue.db);
        let last = records.last().unwrap();
        assert_eq!(last["reason"], reason);
        assert_eq!(last["destination"], "queue_service");
        assert_eq!(last["run"], json!(named));
        assert!(!records.iter().any(|event| event.to_string().contains(&own)));
    }
    // The destinations not built yet, and any other, with the run's token.
    for (head, host) in [
        (
            "CONNECT crates.io:443 HTTP/1.1\r\nHost: crates.io:443".to_owned(),
            "crates.io",
        ),
        (
            "POST /v1/messages HTTP/1.1\r\nHost: api.anthropic.com".to_owned(),
            "api.anthropic.com",
        ),
        // The queue's own path, for another host, goes to that host.
        (
            "POST /queue HTTP/1.1\r\nHost: api.anthropic.com".to_owned(),
            "api.anthropic.com",
        ),
    ] {
        let head = format!("{head}\r\nAuthorization: {BEARER}{own}\r\n{RUN_HEADER}: {run}");
        let (status, body) = broker::exchange(&address, &head, b"", TIMEOUT).unwrap();
        assert_eq!(status, 403, "{host}");
        assert_eq!(
            response(&body).error.unwrap().code,
            ServiceErrorCode::ForbiddenDestination
        );
        let records = denied(&queue.db);
        let last = records.last().unwrap();
        assert_eq!(last["destination"], host);
        assert_eq!(last["run"], run.as_str());
        assert_eq!(last["code"], "forbidden_destination");
    }
    // A request that does not read is refused, with what was read of it.
    let head = format!(
        "CONNECT crates.io:443 HTTP/1.1\r\n{RUN_HEADER}: {run}\r\nTransfer-Encoding: chunked\r\nAuthorization: {BEARER}{own}"
    );
    let (status, body) = broker::exchange(&address, &head, b"", TIMEOUT).unwrap();
    assert_eq!(status, 400);
    assert_eq!(
        response(&body).error.unwrap().code,
        ServiceErrorCode::BadRequest
    );
    let records = denied(&queue.db);
    let last = records.last().unwrap();
    assert_eq!(
        (&last["run"], &last["destination"], &last["reason"]),
        (
            &json!(run.as_str()),
            &json!("crates.io"),
            &json!("malformed")
        )
    );
    assert!(!last.to_string().contains(&own));
    // The health needs no token and records nothing.
    let before = denied(&queue.db).len();
    let health = broker::health(&address, TIMEOUT).unwrap();
    assert_eq!(health["build"], dagq::VERSION);
    let (status, _) = broker::exchange(
        &address,
        &format!("GET {HEALTH_PATH} HTTP/1.1\r\nHost: {address}"),
        b"",
        TIMEOUT,
    )
    .unwrap();
    assert_eq!(status, 200);
    assert_eq!(denied(&queue.db).len(), before);

    // A service that does not answer is the broker's `unreachable`, and the
    // broker opens no queue in its place.
    ok(&queue.db, &["service", "stop"]);
    let (status, body) =
        broker::call(&address, Some(&own), Some(run.as_str()), &show, TIMEOUT).unwrap();
    assert_eq!(status, 502);
    assert_eq!(
        response(&body).error.unwrap().code,
        ServiceErrorCode::Unreachable
    );
}

/// A queue with no broker looks stopped, and so does one whose record and
/// lock file a broker left without holding the lock: a stop clears that
/// record, and a start from a binary that is not there fails.
#[test]
fn a_record_no_broker_holds_looks_stopped_and_a_stop_clears_it() {
    let _test = common::test();
    use dagq::domain::queue_broker::{LOCK_FILE, STATE_FILE};
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(broker::probe(dir.path()).state, ServiceState::Stopped);
    let broker_dir = broker::broker_dir(dir.path());
    std::fs::create_dir_all(&broker_dir).unwrap();
    let record = broker::BrokerRecord {
        pid: u32::MAX - 1,
        build: "x".into(),
        address: "127.0.0.1:1".into(),
        started_at: 0,
    };
    std::fs::write(
        broker_dir.join(STATE_FILE),
        serde_json::to_vec(&record).unwrap(),
    )
    .unwrap();
    std::fs::write(broker_dir.join(LOCK_FILE), b"").unwrap();
    assert_eq!(broker::BrokerRecord::read(dir.path()), Some(record));
    assert_eq!(broker::probe(dir.path()).state, ServiceState::Stopped);
    let control =
        SystemQueueBroker::new(&dir.path().join("queue.db"), Path::new("/nonexistent/dagq"));
    assert_eq!(control.stop(Duration::from_secs(1)).unwrap(), None);
    assert!(broker::BrokerRecord::read(dir.path()).is_none());
    assert!(control.start(Duration::from_secs(1)).is_err());
}

/// No command that reads starts the broker; `up` replaces the broker of the
/// binary before (the `up` after an `install`'s drain) with one of its own
/// binary before the supervisor, and reuses it after; `down` leaves it to
/// the drain or stops it once no supervisor runs.
#[test]
fn up_replaces_the_broker_of_another_build_and_down_stops_it() {
    let mut fixture = common::lifecycle::fixture();
    fixture.options.queue_broker = true;
    fixture.environment.current_exe =
        common::service::owned_executable(fixture.location.db.parent().unwrap());
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let processes = FakeProcesses::default();
    let db = fixture.location.db.clone();
    let _stop = Stop(SystemQueueBroker::new(&db, Path::new("dagq")));

    // Only `up` and the supervisor start it, from the binary that runs
    // them: a command that reads starts none, and a worker's `dagq` (the
    // process that runs a build of a run's work) is refused the broker's
    // start, in client mode and as the worker on the queue.
    ok(&db, &["status"]);
    ok(&db, &["doctor"]);
    let client = {
        use common::service::OwnedByTest;
        use common::{Bounded, WithoutActor};
        std::process::Command::new(env!("CARGO_BIN_EXE_dagq"))
            .without_actor_env()
            .owned_by_test()
            .env(
                dagq::domain::queue_service::SOCKET_ENV,
                service::socket_path(db.parent().unwrap()),
            )
            .env(
                dagq::domain::queue_service::CREDENTIAL_FILE_ENV,
                db.with_file_name("no-credential"),
            )
            .args(["queue-broker", "serve"])
            .bounded_output()
            .unwrap()
    };
    assert!(!client.status.success());
    let refusal: Value = serde_json::from_slice(&client.stderr).unwrap();
    assert_eq!(refusal["queue_service"]["code"], "no_use_case", "{refusal}");
    let worker = common::cli::invoke_as(Some("worker"), &db, &["queue-broker", "serve"]);
    assert!(!worker.status.success());
    let refusal: Value = serde_json::from_slice(&worker.stderr).unwrap();
    assert!(
        refusal.to_string().contains("service.lifecycle"),
        "{refusal}"
    );
    assert_eq!(
        broker::probe(db.parent().unwrap()).state,
        ServiceState::Stopped
    );
    let before = broker_of_the_binary_before(&db);
    let first = common::lifecycle::up(&fixture, &launchd, &processes);
    assert_eq!(first["queue_broker"]["outcome"], "replaced", "{first}");
    assert_eq!(first["queue_broker"]["replaced"]["build"], OLD_BUILD);
    assert_eq!(first["queue_broker"]["broker"]["state"], "running");
    assert_eq!(first["queue_broker"]["broker"]["build"], dagq::VERSION);
    ended(before);
    let second = common::lifecycle::up(&fixture, &launchd, &processes);
    assert_eq!(second["queue_broker"]["outcome"], "reused", "{second}");
    let queue = SqliteQueue::open(&db).unwrap();
    let started = queue
        .latest_queue_event(&["queue_broker_started"])
        .unwrap()
        .unwrap();
    assert_eq!(started.payload["by"], "up");
    assert_eq!(started.payload["build"], dagq::VERSION);
    assert_eq!(started.payload["replaced"]["build"], OLD_BUILD);

    let draining = common::lifecycle::down(&fixture, &launchd, &processes, false, false);
    assert_eq!(
        draining["queue_broker"]["outcome"], "left_to_the_drain",
        "{draining}"
    );
    let asked = queue
        .latest_queue_event(&["queue_broker_stop_requested"])
        .unwrap()
        .unwrap();
    assert_eq!(asked.payload["by"], "down");
    let forced = common::lifecycle::down(&fixture, &launchd, &processes, false, true);
    assert_eq!(forced["queue_broker"]["outcome"], "stopped", "{forced}");
    assert_eq!(
        broker::probe(db.parent().unwrap()).state,
        ServiceState::Stopped
    );
    let stopped = queue
        .latest_queue_event(&["queue_broker_stopped"])
        .unwrap()
        .unwrap();
    assert_eq!(stopped.payload["by"], "down");
    let again = common::lifecycle::down(&fixture, &launchd, &processes, false, false);
    assert!(again.get("queue_broker").is_none(), "{again}");
}

/// A broker in a test's hands: the build that answers, what a start
/// leaves (`None` fails the start), with the starts counted.
#[derive(Default)]
struct FakeBroker {
    running: Mutex<Option<String>>,
    starts_as: Option<String>,
    starts: AtomicU32,
}

impl FakeBroker {
    fn look(build: Option<&String>) -> BrokerProbe {
        BrokerProbe {
            state: if build.is_some() {
                ServiceState::Running
            } else {
                ServiceState::Stopped
            },
            address: build.map(|_| "127.0.0.1:1".to_owned()),
            pid: build.map(|_| 4242),
            build: build.cloned(),
            build_matches: build.map(|build| build == dagq::VERSION),
            started_at: None,
            error: None,
        }
    }
}

impl QueueBrokerControl for FakeBroker {
    fn probe(&self) -> BrokerProbe {
        Self::look(self.running.lock().unwrap().as_ref())
    }
    fn start(&self, _: Duration) -> anyhow::Result<BrokerProbe> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        let Some(build) = self.starts_as.clone() else {
            anyhow::bail!("no binary to start the queue broker with");
        };
        *self.running.lock().unwrap() = Some(build.clone());
        Ok(Self::look(Some(&build)))
    }
    fn stop(&self, _: Duration) -> anyhow::Result<Option<u32>> {
        Ok(self.running.lock().unwrap().take().map(|_| 4242))
    }
}

fn broker_options(control: Arc<FakeBroker>) -> Option<dagq::compose::QueueBrokerOptions> {
    Some(dagq::compose::QueueBrokerOptions {
        executable: "/unused".into(),
        interval: Duration::ZERO,
        start_timeout: TIMEOUT,
        control: Some(dagq::compose::QueueBrokerControlPort(control)),
    })
}

/// A broker that cannot be started is the inbox's attention, told once,
/// and the worker set to the container waits with the broker named as
/// why; a broker that runs again ends the attention, and the supervisor
/// starts one that stopped again.
#[test]
fn the_supervisor_starts_the_broker_again_and_tells_the_inbox_when_it_cannot() {
    use crate::runtime_support::{
        TestWorkspace, VALID_AGENT, fixture, git, supervise_options, supervise_with,
    };
    let (_fixture, repo, db) = fixture();
    std::fs::write(
        repo.join("dagq.toml"),
        "[roles.worker]\nruntime = \"container\"\n",
    )
    .unwrap();
    git(&repo, &["add", "dagq.toml"]);
    git(&repo, &["commit", "-m", "runtimes"]);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let failing = Arc::new(FakeBroker::default());
    let options = |control: Arc<FakeBroker>| dagq::compose::SuperviseOptions {
        queue_broker: broker_options(control),
        ..supervise_options(1, true)
    };
    for _ in 0..2 {
        supervise_with(&db, &repo, &backend, &options(failing.clone())).unwrap();
    }
    let down = events(&db, "queue_broker_down");
    assert_eq!(down.len(), 1, "{down:?}");
    assert_eq!(down[0]["payload"]["reason"], "start_failed");
    let status = ok(&db, &["status"]);
    let attention = status["attention"].as_array().unwrap();
    let entry = attention
        .iter()
        .find(|entry| entry["kind"] == "queue_broker_down")
        .unwrap_or_else(|| panic!("{status}"));
    assert_eq!(entry["next"], "read the queue broker's log and run up");
    let container = attention
        .iter()
        .find(|entry| entry["kind"] == "container_runtime_unavailable")
        .unwrap_or_else(|| panic!("{status}"));
    assert!(
        container["last_error"]
            .as_str()
            .unwrap()
            .contains("queue broker is not running"),
        "{container}"
    );
    assert!(backend.launched.lock().unwrap().is_empty());

    // A binary that starts it: the attention ends; stopped, it is started
    // again, as a restart.
    let starting = Arc::new(FakeBroker {
        starts_as: Some(dagq::VERSION.to_owned()),
        ..FakeBroker::default()
    });
    supervise_with(&db, &repo, &backend, &options(starting.clone())).unwrap();
    assert_eq!(starting.starts.load(Ordering::SeqCst), 1);
    assert!(
        !ok(&db, &["status"])["attention"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["kind"] == "queue_broker_down")
    );
    starting.stop(TIMEOUT).unwrap();
    let running = Running::with_options(
        &db,
        &repo,
        dagq::compose::SuperviseOptions {
            queue_broker: broker_options(starting.clone()),
            ..supervise_options(1, false)
        },
    );
    running.passes(3);
    running.drain();
    assert_eq!(starting.starts.load(Ordering::SeqCst), 2);
    let started = events(&db, "queue_broker_started");
    assert_eq!(started.len(), 2, "{started:?}");
    assert_eq!(started[0]["payload"]["by"], "supervisor");
}

/// What an `install` or an automatic update leaves after the exec: the
/// broker of the binary before, a real process. The supervisor of the new
/// binary replaces it at its first look with a broker of its own binary
/// (the real control), and, the last of the queue at the end of the drain
/// `down` asked for, stops it.
#[test]
fn after_a_handoff_the_new_supervisor_replaces_the_broker_of_the_binary_before() {
    let (_fixture, repo, db) = idle_queue();
    let before = broker_of_the_binary_before(&db);
    let _stop = Stop(SystemQueueBroker::new(&db, Path::new("dagq")));
    let running = Running::with_options(
        &db,
        &repo,
        dagq::compose::SuperviseOptions {
            queue_broker: Some(dagq::compose::QueueBrokerOptions {
                executable: common::service::owned_executable(db.parent().unwrap()),
                interval: Duration::ZERO,
                start_timeout: TIMEOUT,
                control: None,
            }),
            ..crate::runtime_support::supervise_options(1, false)
        },
    );
    let token = supervisor_token(&db);
    {
        let _waiting = common::within(TIMEOUT * 3, "the broker of the binary before replaced");
        while events(&db, "queue_broker_started").is_empty() {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    ended(before);
    running.passes(3);
    let started = events(&db, "queue_broker_started");
    assert_eq!(started.len(), 1, "{started:?}");
    assert_eq!(started[0]["payload"]["by"], "supervisor");
    assert_eq!(started[0]["payload"]["build"], dagq::VERSION);
    assert_eq!(started[0]["payload"]["replaced"]["build"], OLD_BUILD);
    assert_eq!(started[0]["payload"]["restart"], false);
    let now = broker::probe(db.parent().unwrap());
    assert!(now.current(), "{now:?}");
    EventStore::record_queue_event(
        &SqliteQueue::open(&db).unwrap(),
        dagq::domain::EventKind::QueueBrokerStopRequested,
        json!({"supervisors": [token], "by": "down"}),
    )
    .unwrap();
    running.drain();
    assert_eq!(
        broker::probe(db.parent().unwrap()).state,
        ServiceState::Stopped
    );
    let stopped = events(&db, "queue_broker_stopped");
    assert_eq!(stopped.len(), 1, "{stopped:?}");
    assert_eq!(stopped[0]["payload"]["by"], "supervisor");
    assert!(events(&db, "queue_broker_down").is_empty());
}
