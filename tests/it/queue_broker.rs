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
use common::lifecycle::{FakeLaunchd, FakeProcesses, git};
use dagq::application::EventStore;
use dagq::application::queue_broker::TokenFor;
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
        env_role: None,
        queue: None,
        binding: None,
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
        claude_api: dagq::domain::queue_broker::CLAUDE_API_URL.to_owned(),
        host_wide: None,
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
            env_role: None,
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

/// The standard base64 of `text`, as a package manager writes a proxy
/// URL's credentials.
fn base64(text: &str) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in text.as_bytes().chunks(3) {
        let bits = chunk.iter().enumerate().fold(0u32, |bits, (i, byte)| {
            bits | u32::from(*byte) << (16 - 8 * i)
        });
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(bits >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// A host on 127.0.0.1 that echoes what each connection sends, standing in
/// for a registry: no test reaches outside the host.
fn echo_host() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            std::thread::spawn(move || {
                let _ = std::io::copy(&mut &stream, &mut &stream);
            });
        }
    });
    port
}

/// Send `CONNECT target` to the broker at `address` with `credentials` in
/// `Proxy-Authorization`: the status, and the tunnel when it opened or
/// else the refusal.
fn connect(
    address: &str,
    target: &str,
    credentials: &str,
) -> (u16, Result<std::net::TcpStream, ServiceResponse>) {
    use std::io::{BufRead, BufReader, Read, Write};
    let mut stream = std::net::TcpStream::connect(address).unwrap();
    stream.set_read_timeout(Some(TIMEOUT)).unwrap();
    write!(
        stream,
        "CONNECT {target} HTTP/1.1\r\nHost: {target}\r\nProxy-Authorization: {credentials}\r\n\r\n"
    )
    .unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let status: u16 = line.split_whitespace().nth(1).unwrap().parse().unwrap();
    let mut length = 0;
    loop {
        line.clear();
        reader.read_line(&mut line).unwrap();
        if line.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().unwrap();
        }
    }
    if status == 200 {
        assert!(reader.buffer().is_empty());
        return (status, Ok(stream));
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    (status, Err(response(&body)))
}

/// What goes into the tunnel comes back from the echoing host.
fn echoes(mut tunnel: std::net::TcpStream) {
    use std::io::{Read, Write};
    tunnel.write_all(b"ping").unwrap();
    let mut back = [0; 4];
    tunnel.read_exact(&mut back).unwrap();
    assert_eq!(&back, b"ping");
}

/// A `CONNECT` opens a tunnel only to a registry the `dagq.toml` of the
/// main checkout the queue is bound to names, with the run's worker token
/// or its verification's: none without the setting, none for a host a
/// run's clone names, none for the host of an upstream git remote. A
/// verification's token reaches neither the queue service nor the Claude
/// API, and no longer opens a tunnel once revoked. Every refusal is
/// recorded with its run and host, and no token's value.
#[test]
fn a_tunnel_opens_only_to_the_registries_of_the_main_checkout_s_dagq_toml() {
    let _test = common::test();
    let queue = queue();
    let control =
        SystemQueueBroker::new(&queue.db, &common::service::owned_executable(queue.dir()));
    let _stop = Stop(control.clone());
    let address = {
        let _waiting = common::within(TIMEOUT * 2, "the queue broker to start");
        control.start(TIMEOUT).unwrap().address.unwrap()
    };
    let run = queue.run.id().clone();
    let own = token(&queue, &Principal::worker(&run, queue.run.task_id()));
    let verification = service::read_token(
        &broker::issue_verification(queue.dir(), &run, TokenFor::Landing, 1).unwrap(),
    )
    .unwrap();
    // The landing recheck's token of the same run: one of its own.
    let recheck = service::read_token(
        &broker::issue_verification(queue.dir(), &run, TokenFor::Recheck, 1).unwrap(),
    )
    .unwrap();
    assert_ne!(recheck, verification);
    let basic = |token: &str| format!("Basic {}", base64(&format!("{run}:{token}")));
    let registry = echo_host();
    let elsewhere = echo_host();
    let last_denied =
        |reason: &str, host: &str| {
            let records = denied(&queue.db);
            let last = records.last().unwrap().clone();
            assert_eq!(
                (&last["reason"], &last["destination"], &last["run"]),
                (&json!(reason), &json!(host), &json!(run.as_str())),
                "{last}"
            );
            assert!(!records.iter().any(|event| event.to_string().contains(&own)
                || event.to_string().contains(&verification)));
        };

    // No setting (the queue is bound to no repository): every CONNECT is
    // refused.
    let target = format!("127.0.0.1:{registry}");
    let (status, refused) = connect(&address, &target, &basic(&own));
    assert_eq!(status, 403);
    assert_eq!(
        refused.unwrap_err().error.unwrap().code,
        ServiceErrorCode::ForbiddenDestination
    );
    last_denied("not_allowed", "127.0.0.1");

    // The main checkout names the registry and the upstream's host; a
    // run's clone names another host.
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main");
    std::fs::create_dir(&main).unwrap();
    git(&main, &["init", "-q", "--template=", "-b", "main"]);
    git(&main, &["config", "user.name", "test"]);
    git(&main, &["config", "user.email", "test@example.invalid"]);
    git(
        &main,
        &["remote", "add", "origin", "git@upstream.example:o/r.git"],
    );
    std::fs::write(
        main.join("dagq.toml"),
        format!("[egress]\n# the echoing registry\nregistries = [\"127.0.0.1:{registry}\", \"upstream.example\"]\n"),
    )
    .unwrap();
    git(&main, &["add", "."]);
    git(&main, &["commit", "-q", "-m", "seed"]);
    let clone = dir.path().join("clone");
    git(
        &main,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "run",
            clone.to_str().unwrap(),
        ],
    );
    std::fs::write(
        clone.join("dagq.toml"),
        format!("[egress]\nregistries = [\"127.0.0.1:{elsewhere}\"]\n"),
    )
    .unwrap();
    let common_dir = dagq::infrastructure::adapters::git_common_dir(&main).unwrap();
    SqliteQueue::open(&queue.db)
        .unwrap()
        .bind_repository(common_dir.to_str().unwrap())
        .unwrap();

    // The run's token, its verification's and its landing recheck's open
    // the tunnel, in a proxy URL's credentials.
    for credentials in [basic(&own), basic(&verification), basic(&recheck)] {
        let (status, tunnel) = connect(&address, &target, &credentials);
        assert_eq!(status, 200);
        echoes(tunnel.unwrap());
    }
    // Not the host the clone names, nor the upstream's, which says why.
    let (status, _) = connect(&address, &format!("127.0.0.1:{elsewhere}"), &basic(&own));
    assert_eq!(status, 403);
    last_denied("not_allowed", "127.0.0.1");
    let (status, refused) = connect(&address, "upstream.example:443", &basic(&own));
    assert_eq!(status, 403);
    let message = refused.unwrap_err().error.unwrap().message;
    assert!(message.contains("upstream git remote"), "{message}");
    last_denied("upstream_git_host", "upstream.example");
    // A token of no run's opens none.
    let (status, _) = connect(&address, &target, &basic(&"0".repeat(64)));
    assert_eq!(status, 401);
    last_denied("unknown_token", "127.0.0.1");

    // A verification's token reaches neither the queue service nor the
    // Claude API.
    let show = request(UseCase::Show, json!({"id": queue.run.task_id().as_i64()}));
    let (status, body) = broker::call(
        &address,
        Some(&verification),
        Some(run.as_str()),
        &show,
        TIMEOUT,
    )
    .unwrap();
    assert_eq!(status, 401);
    assert_eq!(
        response(&body).error.unwrap().code,
        ServiceErrorCode::Unauthenticated
    );
    last_denied("verification_token", "queue_service");
    let (status, _) = connect(&address, "api.anthropic.com:443", &basic(&verification));
    assert_eq!(status, 403);
    last_denied("builtin_destination", "api.anthropic.com");
    let (status, _) = broker::exchange(
        &address,
        &format!(
            "POST /v1/messages HTTP/1.1\r\nHost: api.anthropic.com\r\nAuthorization: {BEARER}{verification}\r\n{RUN_HEADER}: {run}"
        ),
        b"{}",
        TIMEOUT,
    )
    .unwrap();
    assert_eq!(status, 403);
    last_denied("not_allowed", "api.anthropic.com");

    // Revoked at the verification's end, it opens no tunnel.
    // The landing recheck's is left as it is, until it is revoked.
    assert!(broker::revoke_verification(queue.dir(), &run, TokenFor::Landing).unwrap());
    assert_eq!(
        broker::verification_run(queue.dir(), &verification).unwrap(),
        None
    );
    let (status, _) = connect(&address, &target, &basic(&verification));
    assert_eq!(status, 401);
    last_denied("unknown_token", "127.0.0.1");
    let (status, tunnel) = connect(&address, &target, &basic(&recheck));
    assert_eq!(status, 200);
    echoes(tunnel.unwrap());
    assert!(broker::revoke_verification(queue.dir(), &run, TokenFor::Recheck).unwrap());
    let (status, _) = connect(&address, &target, &basic(&recheck));
    assert_eq!(status, 401);
}

/// The payloads of the queue's `queue_broker_tokens_swept`, read on the DB.
fn swept(db: &Path) -> Vec<Value> {
    rusqlite::Connection::open(db)
        .unwrap()
        .prepare(
            "SELECT payload FROM run_events WHERE kind = 'queue_broker_tokens_swept' ORDER BY id",
        )
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .map(|payload| serde_json::from_str(&payload.unwrap()).unwrap())
        .collect()
}

/// Bind `queue` to a main checkout whose `dagq.toml` names an echoing
/// registry: the checkout's directory, kept while it lives, and the
/// registry's `host:port` a `CONNECT` names.
fn bound_to_a_registry(queue: &common::service::Queue) -> (tempfile::TempDir, String) {
    let registry = echo_host();
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main");
    std::fs::create_dir(&main).unwrap();
    git(&main, &["init", "-q", "--template=", "-b", "main"]);
    git(&main, &["config", "user.name", "test"]);
    git(&main, &["config", "user.email", "test@example.invalid"]);
    std::fs::write(
        main.join("dagq.toml"),
        format!("[egress]\nregistries = [\"127.0.0.1:{registry}\"]\n"),
    )
    .unwrap();
    git(&main, &["add", "."]);
    git(&main, &["commit", "-q", "-m", "seed"]);
    let common_dir = dagq::infrastructure::adapters::git_common_dir(&main).unwrap();
    SqliteQueue::open(&queue.db)
        .unwrap()
        .bind_repository(common_dir.to_str().unwrap())
        .unwrap();
    (dir, format!("127.0.0.1:{registry}"))
}

/// The `Proxy-Authorization` of `run` with `token`.
fn basic(run: &str, token: &str) -> String {
    format!("Basic {}", base64(&format!("{run}:{token}")))
}

/// `recover` of a run whose `integrate` was killed in its verification,
/// while the broker runs and no supervisor does: the landing's token goes
/// with the recovery, recorded with no value, and the broker refuses it
/// from then on; the landing recheck's, which may still run on the run
/// waiting to land again, stays.
#[test]
fn recover_revokes_the_token_of_the_verification_its_run_was_killed_in() {
    let _test = common::test();
    let queue = queue();
    let run = queue.run.id().clone();
    let dead = {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
    };
    let conn = rusqlite::Connection::open(&queue.db).unwrap();
    conn.execute(
        "UPDATE task_runs SET status='integrating' WHERE id=?1",
        [run.as_str()],
    )
    .unwrap();
    conn.execute("DELETE FROM run_leases", []).unwrap();
    conn.execute(
        "INSERT INTO run_leases(run_id,token,pid,heartbeat_at) VALUES (?1,'landing',?2,0)",
        rusqlite::params![run.as_str(), dead],
    )
    .unwrap();
    let issue = |of: TokenFor| {
        let file = broker::issue_verification(queue.dir(), &run, of, 1).unwrap();
        let token = service::read_token(&file).unwrap();
        (file, token)
    };
    let (landing, landing_token) = issue(TokenFor::Landing);
    let (recheck, recheck_token) = issue(TokenFor::Recheck);
    let control =
        SystemQueueBroker::new(&queue.db, &common::service::owned_executable(queue.dir()));
    let _stop = Stop(control.clone());
    let address = {
        let _waiting = common::within(TIMEOUT * 2, "the queue broker to start");
        control.start(TIMEOUT).unwrap().address.unwrap()
    };
    let (_main, target) = bound_to_a_registry(&queue);
    // Before the recovery the landing's token still opens its tunnel.
    let (status, tunnel) = connect(&address, &target, &basic(run.as_str(), &landing_token));
    assert_eq!(status, 200);
    echoes(tunnel.unwrap());

    let recovered = ok(&queue.db, &["recover", run.as_str()]);
    assert_eq!(recovered["outcome"], "recovered", "{recovered}");
    assert_eq!(
        recovered["run"]["status"], "awaiting_integration",
        "{recovered}"
    );
    assert!(!landing.exists());
    assert_eq!(
        broker::verification_run(queue.dir(), &landing_token).unwrap(),
        None
    );
    let (status, _) = connect(&address, &target, &basic(run.as_str(), &landing_token));
    assert_eq!(status, 401);
    assert!(recheck.exists());
    let (status, tunnel) = connect(&address, &target, &basic(run.as_str(), &recheck_token));
    assert_eq!(status, 200);
    echoes(tunnel.unwrap());
    let records = swept(&queue.db);
    assert_eq!(
        records,
        [
            json!({"by": "recover", "revoked": [{"run": run.as_str(), "for": "landing"}], "orphans": 0, "failures": []})
        ]
    );
    assert!(!records[0].to_string().contains(&landing_token));
}

/// What a verification killed in its middle left in the broker's
/// directory goes as the broker starts: the token of a run the queue does
/// not hold and the one of a use its run's status runs no verification
/// in are revoked, recorded with no value, and refused from then on; the
/// landing token of the run that is `integrating` is kept and still opens
/// its tunnel. A broker started again finds nothing more to sweep.
#[test]
fn the_broker_revokes_as_it_starts_the_tokens_no_verification_runs_with() {
    let _test = common::test();
    let queue = queue();
    let run = queue.run.id().clone();
    rusqlite::Connection::open(&queue.db)
        .unwrap()
        .execute(
            "UPDATE task_runs SET status='integrating' WHERE id=?1",
            [run.as_str()],
        )
        .unwrap();
    let issue = |run: &RunId, of: TokenFor| {
        let file = broker::issue_verification(queue.dir(), run, of, 1).unwrap();
        let token = service::read_token(&file).unwrap();
        (file, token)
    };
    let (landing, kept) = issue(&run, TokenFor::Landing);
    let (snapshot, snapshot_token) = issue(&run, TokenFor::Snapshot);
    let gone_run = RunId::new("gone-run").unwrap();
    let (gone, gone_token) = issue(&gone_run, TokenFor::Landing);

    let control =
        SystemQueueBroker::new(&queue.db, &common::service::owned_executable(queue.dir()));
    let _stop = Stop(control.clone());
    let address = {
        let _waiting = common::within(TIMEOUT * 2, "the queue broker to start");
        control.start(TIMEOUT).unwrap().address.unwrap()
    };
    assert!(landing.exists());
    assert_eq!(
        broker::verification_run(queue.dir(), &kept).unwrap(),
        Some(run.clone())
    );
    for (file, token) in [(&snapshot, &snapshot_token), (&gone, &gone_token)] {
        assert!(!file.exists(), "{}", file.display());
        assert_eq!(broker::verification_run(queue.dir(), token).unwrap(), None);
    }
    let records = swept(&queue.db);
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0]["by"], "broker");
    assert_eq!(records[0]["failures"], json!([]));
    let mut revoked = records[0]["revoked"].as_array().unwrap().clone();
    revoked.sort_by_key(|one| one["run"].as_str().unwrap().to_owned());
    assert_eq!(
        revoked,
        [
            json!({"run": run.as_str(), "for": "snapshot"}),
            json!({"run": "gone-run", "for": "landing"})
        ]
    );
    for token in [&kept, &snapshot_token, &gone_token] {
        assert!(!records[0].to_string().contains(token.as_str()));
    }

    // The swept token is refused; the kept one still opens its tunnel.
    let (_main, target) = bound_to_a_registry(&queue);
    let (status, _) = connect(&address, &target, &basic("gone-run", &gone_token));
    assert_eq!(status, 401);
    let (status, _) = connect(&address, &target, &basic(run.as_str(), &snapshot_token));
    assert_eq!(status, 401);
    let (status, tunnel) = connect(&address, &target, &basic(run.as_str(), &kept));
    assert_eq!(status, 200);
    echoes(tunnel.unwrap());

    // Started again, it finds nothing more and records nothing.
    control.stop(TIMEOUT).unwrap();
    {
        let _waiting = common::within(TIMEOUT * 2, "the queue broker to start again");
        control.start(TIMEOUT).unwrap();
    }
    assert_eq!(swept(&queue.db).len(), 1);
    assert!(landing.exists());
}

/// One request the fake Claude API took: its request line, headers and
/// body.
#[derive(Debug, Clone)]
struct Taken {
    line: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Taken {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(own, _)| own.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// The SSE events the fake API streams, in order.
const EVENTS: [&str; 3] = [
    "event: message_start\ndata: {\"type\":\"message_start\"}\n\n",
    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"hi\"}}\n\n",
    "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
];

/// A fake Claude API on a port of 127.0.0.1 that keeps what it took. For
/// `/v1/messages` it streams [`EVENTS`] chunked, the first before and the
/// rest after `go` says so (a broker that held the answer back would never
/// pass the first on); for `?case=auth`, `?case=limit` and `?case=credit`
/// it answers the API's refusal of a login, of the usage limit and of an
/// API key's balance.
fn fake_claude_api() -> (u16, Arc<Mutex<Vec<Taken>>>, std::sync::mpsc::Sender<()>) {
    use std::io::{BufRead, BufReader, Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let taken = Arc::new(Mutex::new(Vec::new()));
    let (go, went) = std::sync::mpsc::channel::<()>();
    let kept = taken.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let mut headers = Vec::new();
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).unwrap();
                let header = header.trim_end().to_owned();
                if header.is_empty() {
                    break;
                }
                let (name, value) = header.split_once(':').unwrap();
                headers.push((name.trim().to_owned(), value.trim().to_owned()));
            }
            let length = headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .map_or(0, |(_, value)| value.parse().unwrap());
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            let line = line.trim_end().to_owned();
            kept.lock().unwrap().push(Taken {
                line: line.clone(),
                headers,
                body,
            });
            let refusal = |status: &str, kind: &str, message: &str| {
                let body = format!(
                    "{{\"type\":\"error\",\"error\":{{\"type\":\"{kind}\",\"message\":\"{message}\"}}}}"
                );
                format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nretry-after: 7\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                )
            };
            let answer = if line.contains("case=auth") {
                refusal(
                    "401 Unauthorized",
                    "authentication_error",
                    "invalid x-api-key",
                )
            } else if line.contains("case=limit") {
                refusal("429 Too Many Requests", "rate_limit_error", "rate limited")
            } else if line.contains("case=credit") {
                refusal(
                    "400 Bad Request",
                    "invalid_request_error",
                    "Your credit balance is too low to access the Anthropic API.",
                )
            } else {
                let chunk = |event: &str| format!("{:x}\r\n{event}\r\n", event.len());
                stream
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: keep-alive\r\nanthropic-ratelimit-unified-status: allowed\r\n\r\n{}",
                            chunk(EVENTS[0])
                        )
                        .as_bytes(),
                    )
                    .unwrap();
                stream.flush().unwrap();
                if went.recv_timeout(TIMEOUT).is_err() {
                    return;
                }
                format!("{}{}0\r\n\r\n", chunk(EVENTS[1]), chunk(EVENTS[2]))
            };
            stream.write_all(answer.as_bytes()).unwrap();
        }
    });
    (port, taken, go)
}

/// Send a Claude API request to the broker at `address` as Claude Code in a
/// container does (`ANTHROPIC_BASE_URL` the broker's `/anthropic`, the run
/// token its `ANTHROPIC_AUTH_TOKEN`, the run in a custom header).
fn claude_request(
    address: &str,
    path: &str,
    token: Option<&str>,
    run: &str,
    body: &str,
) -> std::net::TcpStream {
    use std::io::Write;
    let mut stream = std::net::TcpStream::connect(address).unwrap();
    stream.set_read_timeout(Some(TIMEOUT)).unwrap();
    let authorization = token.map_or_else(String::new, |token| {
        format!("Authorization: {BEARER}{token}\r\n")
    });
    write!(
        stream,
        "POST /anthropic{path} HTTP/1.1\r\nHost: {address}\r\n{authorization}{RUN_HEADER}: {run}\r\nx-api-key: from-the-client\r\nanthropic-version: 2023-06-01\r\nContent-Type: application/json\r\nConnection: keep-alive\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    stream
}

/// Read `stream` to its end: the status and the whole answer.
fn answered(mut stream: std::net::TcpStream) -> (u16, String) {
    use std::io::Read;
    let mut answer = Vec::new();
    stream.read_to_end(&mut answer).unwrap();
    let answer = String::from_utf8(answer).unwrap();
    let status = answer
        .split_whitespace()
        .nth(1)
        .and_then(|status| status.parse().ok())
        .unwrap_or_else(|| panic!("no status: {answer}"));
    (status, answer)
}

/// A Claude API request with its run's own live token goes on to the API
/// with the credential `host.toml` chooses (the long-lived token as the
/// bearer, or the API key in `x-api-key` with no `Authorization`) and none
/// of the run's, and the API's answer comes back as it streams; the API's
/// refusals of a login, of the limit and of a balance come back untouched,
/// once, with the chosen kind only; a request without its run's live token,
/// or while no usable credential is chosen, never reaches the API.
#[test]
fn the_broker_swaps_a_run_s_token_for_the_chosen_claude_credential_and_streams_the_answer() {
    use std::io::Read;
    use std::os::unix::fs::PermissionsExt;
    let _test = common::test();
    let queue = queue();
    start(&queue);
    let (port, taken, go) = fake_claude_api();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let serving = {
        let options = broker::BrokerServeOptions {
            db: queue.db.clone(),
            generators: dagq::infrastructure::clock::system(),
            stop: stop.clone(),
            poll: Duration::from_millis(20),
            owner: None,
            build: "claude-test".to_owned(),
            claude_api: format!("http://127.0.0.1:{port}"),
            host_wide: None,
        };
        std::thread::spawn(move || broker::serve(&options).unwrap())
    };
    let address = {
        let _waiting = common::within(TIMEOUT, "the queue broker to listen");
        loop {
            if let Some(record) = broker::BrokerRecord::read(queue.dir()) {
                break record.address;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    let run = queue.run.id().clone();
    let own = token(&queue, &Principal::worker(&run, queue.run.task_id()));
    let credentials = tempfile::tempdir().unwrap();
    let choose = |kind: &str, secret: &str| {
        let file = credentials.path().join(kind);
        std::fs::write(&file, format!("{secret}\n")).unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::write(
            queue.dir().join("host.toml"),
            format!(
                "[claude_credential]\nkind = \"{kind}\"\nfile = \"{}\"\n",
                file.display()
            ),
        )
        .unwrap();
    };
    let body = r#"{"model":"m","stream":true,"messages":[]}"#;
    let last = || taken.lock().unwrap().last().cloned().unwrap();
    let taken_count = || taken.lock().unwrap().len();

    for (kind, secret, sent, absent) in [
        (
            "setup_token",
            "sk-ant-oat01-long-lived",
            ("authorization", "Bearer sk-ant-oat01-long-lived"),
            "x-api-key",
        ),
        (
            "api_key",
            "sk-ant-api03-key",
            ("x-api-key", "sk-ant-api03-key"),
            "authorization",
        ),
    ] {
        choose(kind, secret);
        let mut stream = claude_request(
            &address,
            "/v1/messages?beta=true",
            Some(&own),
            run.as_str(),
            body,
        );
        // The first event comes through before the API sends the rest.
        let mut seen = Vec::new();
        let mut buffer = [0u8; 4096];
        while !String::from_utf8_lossy(&seen).contains(EVENTS[0]) {
            let read = stream.read(&mut buffer).unwrap();
            assert!(read > 0, "{}", String::from_utf8_lossy(&seen));
            seen.extend_from_slice(&buffer[..read]);
        }
        go.send(()).unwrap();
        stream.read_to_end(&mut seen).unwrap();
        let answer = String::from_utf8(seen).unwrap();
        let (head, streamed) = answer.split_once("\r\n\r\n").unwrap();
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert!(head.contains("Transfer-Encoding: chunked"), "{head}");
        assert!(
            head.contains("anthropic-ratelimit-unified-status: allowed"),
            "{head}"
        );
        assert!(head.contains("Connection: close"), "{head}");
        assert!(!head.contains("keep-alive"), "{head}");
        let at = |event: &str| streamed.find(event).unwrap_or_else(|| panic!("{streamed}"));
        assert!(at(EVENTS[0]) < at(EVENTS[1]) && at(EVENTS[1]) < at(EVENTS[2]));
        assert!(streamed.ends_with("0\r\n\r\n"), "{streamed:?}");

        let request = last();
        assert_eq!(request.line, "POST /v1/messages?beta=true HTTP/1.1");
        assert_eq!(request.header(sent.0), Some(sent.1), "{request:?}");
        assert_eq!(request.header(absent), None, "{request:?}");
        assert_eq!(request.header(RUN_HEADER), None);
        assert_eq!(request.header("anthropic-version"), Some("2023-06-01"));
        assert_eq!(request.body, body.as_bytes());
        assert!(
            !request
                .headers
                .iter()
                .any(|(_, value)| value.contains(&own) || value == "from-the-client"),
            "the run's token or the client's credential left the broker: {request:?}"
        );

        // The API's refusals come back untouched, sent once with the chosen
        // kind only.
        for (case, status, says) in [
            ("auth", 401, "invalid x-api-key"),
            ("limit", 429, "rate_limit_error"),
            ("credit", 400, "credit balance is too low"),
        ] {
            let before = taken_count();
            let (code, answer) = answered(claude_request(
                &address,
                &format!("/v1/messages?case={case}"),
                Some(&own),
                run.as_str(),
                body,
            ));
            assert_eq!(code, status, "{answer}");
            assert!(answer.contains(says), "{answer}");
            assert!(answer.contains("retry-after: 7"), "{answer}");
            assert_eq!(taken_count(), before + 1, "{case} was sent again");
            assert_eq!(last().header(sent.0), Some(sent.1));
            assert_eq!(last().header(absent), None);
        }
    }

    // Without the run's own live token, never sent on.
    let ghost = token(
        &queue,
        &Principal::worker(&RunId::new("ghost-run").unwrap(), TaskId::new(99)),
    );
    let gone = Principal::worker(&RunId::new("revoked-run").unwrap(), TaskId::new(98));
    let revoked = token(&queue, &gone);
    assert!(service::revoke(queue.dir(), &gone.actor_id).unwrap());
    let before = taken_count();
    for (bearer, reason) in [
        (None, "missing_token"),
        (Some(ghost.as_str()), "other_run"),
        (Some(revoked.as_str()), "unknown_token"),
    ] {
        let (code, answer) = answered(claude_request(
            &address,
            "/v1/messages",
            bearer,
            run.as_str(),
            body,
        ));
        assert_eq!(code, 401, "{answer}");
        let record = denied(&queue.db).last().cloned().unwrap();
        assert_eq!(record["reason"], reason);
        assert_eq!(record["destination"], "api.anthropic.com");
        assert_eq!(record["run"], run.as_str());
    }
    // No usable credential chosen: refused with what to choose, never sent
    // with a kind of the broker's own.
    std::fs::write(queue.dir().join("host.toml"), "[push]\ncommand = []\n").unwrap();
    let (code, answer) = answered(claude_request(
        &address,
        "/v1/messages",
        Some(&own),
        run.as_str(),
        body,
    ));
    assert_eq!(code, 401, "{answer}");
    assert!(
        answer.contains("setup_token") && answer.contains("api_key"),
        "{answer}"
    );
    assert_eq!(
        denied(&queue.db).last().unwrap()["reason"],
        "claude_credential_not_selected"
    );
    choose("api_key", "sk-ant-api03-key");
    std::fs::set_permissions(
        credentials.path().join("api_key"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    let (code, _) = answered(claude_request(
        &address,
        "/v1/messages",
        Some(&own),
        run.as_str(),
        body,
    ));
    assert_eq!(code, 401);
    assert_eq!(
        denied(&queue.db).last().unwrap()["reason"],
        "claude_credential_not_private"
    );
    assert_eq!(taken_count(), before, "a refused request reached the API");
    for record in denied(&queue.db) {
        let record = record.to_string();
        assert!(
            !record.contains(&own) && !record.contains("sk-ant-"),
            "{record}"
        );
    }

    stop.store(true, Ordering::SeqCst);
    let _waiting = common::within(TIMEOUT, "the queue broker to stop once asked to");
    serving.join().unwrap();
}
