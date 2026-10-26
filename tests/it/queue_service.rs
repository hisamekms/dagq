//! The queue service on the host (docs/design/queue-service.md,
//! ADR-t1233-1, ADR-t1233-4, ADR-t1233-5): the `dagq` binary's service
//! answers `ask`, `show` and `note` on its unix socket for the principal a
//! token names, refuses and records what the policy refuses on its side,
//! refuses a caller without a principal and one of another API version;
//! `service start|stop|status`, `status` and `doctor` report it; `up` and
//! `down` start and stop it, and a supervisor starts it again and holds
//! its claims while it cannot, its outage recorded beside the queue
//! (ADR-t3102-3).

use crate::common::{self, Bounded, WithoutActor};
use crate::runtime_support::service::{Running, idle_queue, supervisor_token};
use common::service::{call, call_as_role, events, queue, start, token};

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::Duration,
};

use common::cli::{invoke, ok};
use common::lifecycle::{FakeLaunchd, FakeProcesses};
use dagq::{
    application::{EventStore, RunReads},
    domain::{
        ActorContext,
        queue_service::{API_VERSION, Principal, ServiceErrorCode, ServiceRequest, UseCase},
    },
    infrastructure::{
        queue_service::{self as service, SystemQueueService},
        sqlite::SqliteQueue,
    },
};
use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(10);

#[test]
fn the_service_answers_its_principals_and_refuses_the_rest_on_its_side() {
    let queue = queue();
    let started = start(&queue);
    assert_eq!(started["outcome"], "started", "{started}");
    assert_eq!(started["service"]["state"], "running");
    assert_eq!(started["service"]["build"], dagq::VERSION);
    // Once running, another start reuses it, and a second service of the
    // same queue is refused.
    assert_eq!(start(&queue)["outcome"], "reused");
    let second = invoke(&queue.db, &["service", "serve"]);
    assert!(!second.status.success());
    assert!(
        String::from_utf8_lossy(&second.stderr).contains("already runs"),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    let socket = service::socket_path(queue.dir());
    let mode = fs::metadata(&socket).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);

    let task = queue.run.task_id();
    let worker = Principal::worker(queue.run.id(), task);
    let worker_token = token(&queue, &worker);
    let review = Principal::of(&ActorContext::review_job(queue.run.id(), 1));
    let review_token = token(&queue, &review);

    // `show` is `dagq show`'s output, for every role.
    let shown = call(
        &queue,
        Some(&worker_token),
        UseCase::Show,
        json!({"id": task}),
    );
    assert_eq!(shown["ok"], true, "{shown}");
    assert_eq!(shown["result"], ok(&queue.db, &["show", &task.to_string()]));
    let full = call(
        &queue,
        Some(&review_token),
        UseCase::Show,
        json!({"id": task, "full": true}),
    );
    assert_eq!(
        full["result"],
        ok(&queue.db, &["show", &task.to_string(), "--full"])
    );

    // The worker asks on its own run, recorded as itself.
    let asked = call(
        &queue,
        Some(&worker_token),
        UseCase::Ask,
        json!({"kind": "worker_question", "because": "scope", "topics": ["design_choice"],
               "question": "which way?", "run_id": queue.run.id()}),
    );
    assert_eq!(asked["ok"], true, "{asked}");
    let asks = ok(&queue.db, &["asks", "--all"])["asks"].clone();
    assert_eq!(asks[0]["asked_by"], "worker", "{asks}");
    assert_eq!(asks[0]["run_id"], json!(queue.run.id()));
    let opened = events(&queue.db, "ask_opened");
    assert_eq!(opened[0]["actor"]["role"], "worker", "{opened:?}");

    // Not on another run: refused by the service and recorded as the
    // worker, whatever it names.
    let refused = call(
        &queue,
        Some(&worker_token),
        UseCase::Ask,
        json!({"kind": "worker_question", "because": "scope", "topics": ["design_choice"],
               "question": "q", "run_id": "someone-else"}),
    );
    assert_eq!(
        refused["error"]["code"], "authorization_denied",
        "{refused}"
    );
    // A note on its task; a review job notes nothing.
    let noted = call(
        &queue,
        Some(&worker_token),
        UseCase::Note,
        json!({"task": task, "text": "seen", "kind": "observation"}),
    );
    assert_eq!(noted["ok"], true, "{noted}");
    let denied = call(
        &queue,
        Some(&review_token),
        UseCase::Note,
        json!({"task": task, "text": "x"}),
    );
    assert_eq!(denied["error"]["code"], "authorization_denied");
    let notes = ok(&queue.db, &["notes", "--task", &task.to_string()]);
    assert_eq!(notes["notes"].as_array().unwrap().len(), 1, "{notes}");
    assert_eq!(notes["notes"][0]["payload"]["by"], "worker", "{notes}");
    assert_eq!(notes["notes"][0]["actor"]["role"], "worker", "{notes}");
    let denials = events(&queue.db, "authorization_denied");
    let refusals: Vec<(&Value, &Value)> = denials
        .iter()
        .map(|event| (&event["payload"]["role"], &event["payload"]["capability"]))
        .collect();
    assert_eq!(
        refusals,
        [
            (&json!("worker"), &json!("ask.open")),
            (&json!("review-job"), &json!("note.write")),
        ]
    );

    // No token and no role from this user's process: the user.
    let user = call(&queue, None, UseCase::Show, json!({"id": task}));
    assert_eq!(user["ok"], true, "{user}");
    // No principal: no token from an environment that names a role, one
    // never issued, one revoked.
    let response = call_as_role(&queue, "worker", UseCase::Show, json!({"id": 1}));
    assert_eq!(response["error"]["code"], "unauthenticated", "{response}");
    let response = call(
        &queue,
        Some(&"f".repeat(64)),
        UseCase::Show,
        json!({"id": 1}),
    );
    assert_eq!(response["error"]["code"], "unauthenticated", "{response}");
    service::revoke(queue.dir(), &review.actor_id).unwrap();
    let revoked = call(
        &queue,
        Some(&review_token),
        UseCase::Show,
        json!({"id": task}),
    );
    assert_eq!(revoked["error"]["code"], "unauthenticated");
    let reasons: Vec<Value> = events(&queue.db, "queue_service_unauthenticated")
        .iter()
        .map(|event| event["payload"]["reason"].clone())
        .collect();
    assert_eq!(
        reasons,
        [
            json!("missing_token"),
            json!("unknown_token"),
            json!("unknown_token")
        ]
    );
    // No record names a token.
    let all = ok(&queue.db, &["events", "--all", "--full"]).to_string();
    assert!(!all.contains(&worker_token) && !all.contains(&review_token));

    // Another API version is refused before anything else.
    let response = service::call(
        &socket,
        &ServiceRequest {
            api_version: API_VERSION + 1,
            token: Some(worker_token.clone()),
            env_role: None,
            queue: None,
            binding: None,
            use_case: UseCase::Show,
            params: json!({"id": task}),
        },
        TIMEOUT,
    )
    .unwrap();
    let error = response.error.unwrap();
    assert_eq!(error.code, ServiceErrorCode::ApiVersionMismatch);
    assert_eq!(response.api_version, API_VERSION);
    assert!(dagq::domain::queue_service::understands(
        &service::call(
            &socket,
            &ServiceRequest {
                api_version: API_VERSION,
                token: None,
                env_role: None,
                queue: None,
                binding: None,
                use_case: UseCase::Hello,
                params: Value::Null,
            },
            TIMEOUT,
        )
        .unwrap()
    ));

    // A run that ended ends its worker's token.
    rusqlite::Connection::open(&queue.db)
        .unwrap()
        .execute(
            "UPDATE task_runs SET status='failed' WHERE id=?1",
            [queue.run.id().as_str()],
        )
        .unwrap();
    let ended = call(
        &queue,
        Some(&worker_token),
        UseCase::Show,
        json!({"id": task}),
    );
    assert_eq!(ended["error"]["code"], "unauthenticated", "{ended}");

    // Stopped, nothing answers, and a client does not fall back to the DB.
    let stopped = ok(&queue.db, &["service", "stop"]);
    assert_eq!(stopped["outcome"], "stopped");
    assert!(
        service::call(
            &socket,
            &ServiceRequest {
                api_version: API_VERSION,
                token: Some(worker_token),
                env_role: None,
                queue: None,
                binding: None,
                use_case: UseCase::Show,
                params: json!({"id": task}),
            },
            TIMEOUT,
        )
        .is_err()
    );
    assert_eq!(
        ok(&queue.db, &["service", "stop"])["outcome"],
        "not_running"
    );
    let by: Vec<Value> = [
        events(&queue.db, "queue_service_started"),
        events(&queue.db, "queue_service_stopped"),
    ]
    .concat()
    .iter()
    .map(|event| event["payload"]["by"].clone())
    .collect();
    assert_eq!(by, [json!("service start"), json!("service stop")]);
}

#[test]
fn status_doctor_and_service_status_report_the_service() {
    let queue = queue();
    // `status` is the service's to answer: with none running it says so
    // (ADR-t3102-3 decision 2); the control side's reports say stopped.
    let status = std::process::Command::new(env!("CARGO_BIN_EXE_dagq"))
        .without_actor_env()
        .arg("--db")
        .arg(&queue.db)
        .arg("status")
        .bounded_output()
        .unwrap();
    assert!(!status.status.success());
    let refused: Value = serde_json::from_slice(&status.stderr).unwrap();
    assert_eq!(refused["queue_service"]["code"], "unreachable", "{refused}");
    for args in [&["doctor"][..], &["service", "status"]] {
        let report = ok(&queue.db, args);
        let view = if args == ["service", "status"] {
            &report
        } else {
            &report["queue_service"]
        };
        assert_eq!(view["state"], "stopped", "{args:?} {view}");
        // `service status` names the socket of the DB a link names.
        let dir = if args == ["service", "status"] {
            queue.dir().canonicalize().unwrap()
        } else {
            queue.dir().to_path_buf()
        };
        assert_eq!(view["socket"], json!(service::socket_path(&dir)));
        assert_eq!(view["pid"], Value::Null);
        assert_eq!(view["client_api_version"], API_VERSION);
    }
    // `doctor` reads the attention from the DB; `service status` opens no
    // queue, so it does not know it, and says no outage is recorded.
    assert_eq!(
        ok(&queue.db, &["doctor"])["queue_service"]["attention"],
        false
    );
    let unreached = ok(&queue.db, &["service", "status"]);
    assert_eq!(unreached["attention"], Value::Null, "{unreached}");
    assert_eq!(unreached.get("outage"), Some(&Value::Null), "{unreached}");
    assert!(unreached["next"].as_str().unwrap().contains("dagq up"));
    let started = start(&queue);
    for args in [&["status"][..], &["doctor"], &["service", "status"]] {
        let report = ok(&queue.db, args);
        let view = if args == ["service", "status"] {
            &report
        } else {
            &report["queue_service"]
        };
        assert_eq!(view["state"], "running", "{args:?} {view}");
        assert_eq!(view["pid"], started["service"]["pid"]);
        assert_eq!(view["build"], dagq::VERSION);
        assert_eq!(view["build_matches"], true);
        assert_eq!(view["api_version"], API_VERSION);
        assert_eq!(view["attention"], false, "{args:?} {view}");
    }
}

/// `dagq service status` opens no queue (ADR-t3102-2 decision 1): while
/// the service runs it reads the attention through the service, in the
/// shape it always had; once
/// the service is gone it answers from the socket, the service's record
/// and the outage its supervisor recorded outside the DB, with what to do
/// next, even where no queue opens (ADR-t3102-3 decisions 1 and 2).
#[test]
fn service_status_opens_no_queue_whether_the_service_answers_or_not() {
    let queue = queue();
    let started = start(&queue);
    let running = ok(&queue.db, &["service", "status"]);
    let keys = |view: &Value| {
        let mut keys: Vec<String> = view.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        keys
    };
    assert_eq!(
        keys(&running),
        [
            "api_version",
            "attention",
            "build",
            "build_matches",
            "client_api_version",
            "min_api_version",
            "pid",
            "socket",
            "started_at",
            "state"
        ],
        "{running}"
    );
    assert_eq!(running["state"], "running");
    assert_eq!(running["pid"], started["service"]["pid"]);
    assert_eq!(running["attention"], false);

    // A queue's directory whose DB does not open, with no service and the
    // outage its supervisor recorded beside it.
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("queue.db");
    fs::write(&db, "not a queue").unwrap();
    let unrecorded = ok(&db, &["service", "status"]);
    assert_eq!(unrecorded["state"], "stopped", "{unrecorded}");
    assert_eq!(unrecorded["attention"], Value::Null);
    assert_eq!(unrecorded.get("outage"), Some(&Value::Null));
    assert_eq!(
        unrecorded["next"],
        dagq::application::queue_service::UNREACHED_NEXT
    );
    let outage = dagq::domain::queue_service::ServiceOutage {
        since: 1_700_000_000,
        reason: "restart_limit".to_owned(),
        message: "the service stopped 3 times".to_owned(),
        supervisor: Some("s1".to_owned()),
        pushed: true,
    };
    fs::write(
        dir.path().join(dagq::domain::queue_service::OUTAGE_FILE),
        serde_json::to_vec(&outage).unwrap(),
    )
    .unwrap();
    let recorded = ok(&db, &["service", "status"]);
    assert_eq!(recorded["state"], "stopped", "{recorded}");
    assert_eq!(recorded["outage"], json!(outage));
    assert_eq!(
        recorded["next"],
        dagq::application::queue_service::UNREACHED_NEXT
    );
    assert_eq!(fs::read(&db).unwrap(), b"not a queue");
    // Where no DB is at all, it answers the same.
    fs::remove_file(&db).unwrap();
    let no_db = ok(&db, &["service", "status"]);
    assert_eq!(no_db["state"], "stopped", "{no_db}");
    assert_eq!(no_db["outage"], json!(outage));
    assert!(!db.exists());
}

/// A supervisor that reaches its queue through the service's socket
/// (ADR-t3102-3): one that cannot start the service claims nothing and
/// writes nothing to the queue, records the outage beside the queue,
/// outside the DB, and sends it through `[push]` once, however many passes
/// fail. Once the service runs, the outage becomes the queue's
/// `queue_service_down` (when it began and ended) with the start after it,
/// so the attention ends with it, and the claim is the supervisor's, as its
/// own actor, through the service.
#[test]
fn the_supervisor_records_the_outage_outside_the_queue_and_holds_its_claims() {
    use crate::runtime_support::{
        TestWorkspace, VALID_AGENT, fixture, supervise_options, supervise_with,
    };
    let (_fixture, repo, db) = fixture();
    let queue_dir = db.canonicalize().unwrap().parent().unwrap().to_path_buf();
    let inbox = queue_dir.join("pushed");
    fs::create_dir(&inbox).unwrap();
    let script = queue_dir.join("push.sh");
    common::template::script_env(
        &script,
        "#!/bin/sh\ncat > \"$STUB_INBOX/$DAGQ_PUSH_KIND-$(ls \"$STUB_INBOX\" | wc -l | tr -d ' ').json\"\n",
        &[("STUB_INBOX", inbox.to_str().unwrap())],
    );
    fs::write(
        queue_dir.join("host.toml"),
        format!(
            "[push]\ncommand = [\"{}\"]\ntimeout_secs = 10\n",
            script.display()
        ),
    )
    .unwrap();
    let pushed_down = || {
        fs::read_dir(&inbox)
            .unwrap()
            .filter(|entry| {
                let name = entry.as_ref().unwrap().file_name();
                name.to_str().unwrap().starts_with("queue_service_down-")
            })
            .count()
    };
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    // `[push]` reaches the supervisor through its daily reports' port, given
    // only while the service is out of reach: a report job would hold a
    // `--once` pass that claims.
    let options = |executable: &Path, push: bool| dagq::compose::SuperviseOptions {
        queue_service: Some(dagq::compose::QueueServiceOptions {
            executable: executable.to_path_buf(),
            interval: Duration::ZERO,
            start_timeout: TIMEOUT,
            control: None,
        }),
        service_route: dagq::compose::ServiceRoute::Socket,
        report_daily: push,
        host_config: Some(queue_dir.join("no host-wide file.toml")),
        ..supervise_options(1, true)
    };
    let control = SystemQueueService::new(&db, Path::new("dagq"));
    struct Stop(SystemQueueService);
    impl Drop for Stop {
        fn drop(&mut self) {
            use dagq::application::queue_service::QueueServiceControl;
            let _ = self.0.stop(TIMEOUT);
        }
    }
    let _stop = Stop(control.clone());
    let read = || SqliteQueue::open(&db).unwrap();
    let outage = queue_dir.join(dagq::domain::queue_service::OUTAGE_FILE);

    // A binary that is not there: no service, nothing claimed, nothing
    // written to the queue, the outage beside it and sent once.
    let missing = repo.join("no-dagq");
    // The supervisor's process never opens the queue: the outage goes to
    // its file and `[push]`, and the rest through the service.
    let refused = dagq::infrastructure::sqlite::refuse_opening(&db);
    let unreached = supervise_with(&db, &repo, &backend, &options(&missing, true)).unwrap();
    drop(refused);
    assert_eq!(
        unreached["outcome"], "queue_service_unreachable",
        "{unreached}"
    );
    assert_eq!(unreached["reason"], "start_failed", "{unreached}");
    assert!(RunReads::all_runs(&read()).unwrap().is_empty());
    assert!(read().supervisors().unwrap().is_empty());
    assert!(events(&db, "queue_service_down").is_empty());
    let recorded: Value = serde_json::from_slice(&fs::read(&outage).unwrap()).unwrap();
    assert_eq!(recorded["reason"], "start_failed", "{recorded}");
    assert_eq!(recorded["pushed"], true, "{recorded}");
    assert_eq!(pushed_down(), 1);
    // Told once, however many passes fail.
    supervise_with(&db, &repo, &backend, &options(&missing, true)).unwrap();
    assert_eq!(pushed_down(), 1);
    let again: Value = serde_json::from_slice(&fs::read(&outage).unwrap()).unwrap();
    assert_eq!(again["since"], recorded["since"]);

    // The binary: the service runs, the outage is recorded and ends, the
    // task is claimed through the service as the supervisor's own actor.
    // The tests' binary, with the service it starts stopping once this test
    // process is gone (task 1352).
    let dagq = common::service::owned_executable(db.parent().unwrap());
    let refused = dagq::infrastructure::sqlite::refuse_opening(&db);
    let finished = supervise_with(&db, &repo, &backend, &options(&dagq, false)).unwrap();
    drop(refused);
    assert_eq!(finished["outcome"], "finished", "{finished}");
    assert!(!outage.exists());
    let down = events(&db, "queue_service_down");
    assert_eq!(down.len(), 1, "{down:?}");
    assert_eq!(down[0]["payload"]["reason"], "start_failed");
    assert_eq!(down[0]["payload"]["since"], recorded["since"]);
    assert!(down[0]["payload"]["until"].as_i64() >= recorded["since"].as_i64());
    // The inbox's watch reads it among the attention events.
    let attention = ok(&db, &["events"]);
    assert!(
        attention["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["kind"] == "queue_service_down"),
        "{attention}"
    );
    let started = events(&db, "queue_service_started");
    assert_eq!(started.len(), 1);
    assert_eq!(started[0]["payload"]["by"], "supervisor");
    assert_eq!(started[0]["payload"]["build"], dagq::VERSION);
    assert!(started[0]["id"].as_i64() > down[0]["id"].as_i64());
    let status = ok(&db, &["status"]);
    assert!(
        !status["attention"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["kind"] == "queue_service_down"),
        "{status}"
    );
    assert_eq!(status["queue_service"]["state"], "running");
    assert_eq!(RunReads::all_runs(&read()).unwrap().len(), 1);
    let claimed = events(&db, "run_claimed");
    assert_eq!(
        claimed[0]["actor"]["id"],
        format!("supervisor:{}", std::process::id()),
        "{claimed:?}"
    );
    // Stopped by a person, started again at the next look.
    {
        use dagq::application::queue_service::QueueServiceControl;
        assert!(control.stop(TIMEOUT).unwrap().is_some());
    }
    supervise_with(&db, &repo, &backend, &options(&dagq, false)).unwrap();
    assert_eq!(events(&db, "queue_service_started").len(), 2);
}

/// `up` starts the service before the supervisor and reuses it after;
/// `down` stops it once no supervisor runs.
#[test]
fn up_starts_the_service_and_down_stops_it() {
    let mut fixture = common::lifecycle::fixture();
    fixture.options.queue_service = true;
    fixture.environment.current_exe =
        common::service::owned_executable(fixture.location.db.parent().unwrap());
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let processes = FakeProcesses::default();
    let db = fixture.location.db.clone();
    struct Down(PathBuf);
    impl Drop for Down {
        fn drop(&mut self) {
            let _ = invoke(&self.0, &["service", "stop"]);
        }
    }
    let _down = Down(db.clone());

    let first = common::lifecycle::up(&fixture, &launchd, &processes);
    assert_eq!(first["queue_service"]["outcome"], "started", "{first}");
    assert_eq!(first["queue_service"]["service"]["state"], "running");
    let second = common::lifecycle::up(&fixture, &launchd, &processes);
    assert_eq!(second["queue_service"]["outcome"], "reused", "{second}");
    let queue = SqliteQueue::open(&db).unwrap();
    let started = queue
        .latest_queue_event(&["queue_service_started"])
        .unwrap()
        .unwrap();
    assert_eq!(started.payload["by"], "up");

    // The supervisor `up` registered is this process: `down` drains it,
    // leaves the service to the drain, and a `--force` stops both.
    let draining = common::lifecycle::down(&fixture, &launchd, &processes, false, false);
    assert_eq!(
        draining["queue_service"]["outcome"], "left_to_the_drain",
        "{draining}"
    );
    let asked = queue
        .latest_queue_event(&["queue_service_stop_requested"])
        .unwrap()
        .unwrap();
    assert_eq!(asked.payload["by"], "down");
    let forced = common::lifecycle::down(&fixture, &launchd, &processes, false, true);
    assert_eq!(forced["queue_service"]["outcome"], "stopped", "{forced}");
    assert_eq!(
        service::probe(db.parent().unwrap()).state,
        dagq::domain::queue_service::ServiceState::Stopped
    );
    let stopped = queue
        .latest_queue_event(&["queue_service_stopped"])
        .unwrap()
        .unwrap();
    assert_eq!(stopped.payload["by"], "down");
    // Nothing more is said of a service that does not run.
    let again = common::lifecycle::down(&fixture, &launchd, &processes, false, false);
    assert!(again.get("queue_service").is_none(), "{again}");
}

fn request_stop(db: &Path, tokens: &[&str]) {
    EventStore::record_queue_event(
        &SqliteQueue::open(db).unwrap(),
        dagq::domain::EventKind::QueueServiceStopRequested,
        json!({"supervisors": tokens, "by": "down"}),
    )
    .unwrap();
}

/// The normal `down` (ADR-t1233-4 decision 1): the supervisors drain, and
/// the last one a `queue_service_stop_requested` names stops the service
/// at the end of its drain; one the request does not name leaves it.
#[test]
fn the_last_supervisor_down_asked_stops_the_service_when_its_drain_ends() {
    let (_fixture, repo, db) = idle_queue();
    // The service the supervisor starts stops once this test process is
    // gone (task 1352).
    let dagq = common::service::owned_executable(db.parent().unwrap());
    let queue_dir = db.parent().unwrap().to_path_buf();
    let running = |state| service::probe(&queue_dir).state == state;
    use dagq::domain::queue_service::ServiceState::{Running as Up, Stopped};

    // Not named: the drain ends and the service runs on.
    let supervisor = Running::start(&db, &repo, &dagq, None);
    let token = supervisor_token(&db);
    supervisor.passes(2);
    assert!(running(Up));
    request_stop(&db, &["another-supervisor"]);
    supervisor.drain();
    assert!(running(Up));
    assert!(events(&db, "queue_service_stopped").is_empty());

    // Named: the service ends with the drain, recorded as the supervisor's.
    let supervisor = Running::start(&db, &repo, &dagq, None);
    let second = supervisor_token(&db);
    assert_ne!(second, token);
    supervisor.passes(2);
    let pid = service::probe(&queue_dir).pid.unwrap();
    request_stop(&db, &[&second]);
    supervisor.drain();
    assert!(running(Stopped));
    let stopped = events(&db, "queue_service_stopped");
    assert_eq!(stopped.len(), 1, "{stopped:?}");
    assert_eq!(stopped[0]["payload"]["by"], "supervisor");
    assert_eq!(stopped[0]["payload"]["supervisor"], json!(second));
    assert_eq!(stopped[0]["payload"]["pid"], json!(pid));
}

/// A service in a test's hands: what each look finds and what a start
/// leaves, with the starts counted.
#[derive(Default)]
struct FakeService {
    /// The build that answers now; `None` is none.
    running: std::sync::Mutex<Option<String>>,
    /// The build a start leaves answering; `None` dies at once.
    starts_as: Option<String>,
    starts: std::sync::atomic::AtomicU32,
}

impl FakeService {
    fn look(build: Option<&String>) -> dagq::application::queue_service::ServiceProbe {
        use dagq::domain::queue_service::ServiceState;
        dagq::application::queue_service::ServiceProbe {
            state: if build.is_some() {
                ServiceState::Running
            } else {
                ServiceState::Stopped
            },
            socket: PathBuf::from("/fake/queue.sock"),
            pid: build.map(|_| 4242),
            build: build.cloned(),
            api_version: build.map(|_| API_VERSION),
            min_api_version: build.map(|_| API_VERSION),
            build_matches: build.map(|build| build == dagq::VERSION),
            started_at: None,
            error: None,
        }
    }

    fn starts(&self) -> u32 {
        self.starts.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl dagq::application::queue_service::QueueServiceControl for FakeService {
    fn probe(&self) -> dagq::application::queue_service::ServiceProbe {
        Self::look(self.running.lock().unwrap().as_ref())
    }
    fn start(&self, _: Duration) -> anyhow::Result<dagq::application::queue_service::ServiceProbe> {
        self.starts
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        // A service that dies at once answered its start all the same.
        let answered = self
            .starts_as
            .clone()
            .unwrap_or_else(|| dagq::VERSION.to_owned());
        *self.running.lock().unwrap() = self.starts_as.clone();
        Ok(Self::look(Some(&answered)))
    }
    fn stop(&self, _: Duration) -> anyhow::Result<Option<u32>> {
        Ok(self.running.lock().unwrap().take().map(|_| 4242))
    }
}

/// A service that dies after each start is started 3 times in the window,
/// then left to a person: the outage recorded beside the queue with
/// `restart_limit`, once, and nothing of it in the queue while it lasts.
#[test]
fn a_service_that_keeps_dying_is_left_to_a_person_after_three_starts() {
    let (_fixture, repo, db) = idle_queue();
    let fake = std::sync::Arc::new(FakeService::default());
    let supervisor = Running::start(&db, &repo, Path::new("/unused"), Some(fake.clone()));
    let outage = db
        .parent()
        .unwrap()
        .join(dagq::domain::queue_service::OUTAGE_FILE);
    let _waiting = common::within(TIMEOUT * 3, "the outage's record");
    while !outage.exists() {
        std::thread::sleep(Duration::from_millis(20));
    }
    supervisor.passes(5);
    supervisor.drain();
    assert_eq!(fake.starts(), 3);
    let started = events(&db, "queue_service_started");
    let restarts: Vec<&Value> = started
        .iter()
        .map(|event| &event["payload"]["restart"])
        .collect();
    assert_eq!(restarts, [&json!(false), &json!(true), &json!(true)]);
    let recorded: Value = serde_json::from_slice(&fs::read(&outage).unwrap()).unwrap();
    assert_eq!(recorded["reason"], "restart_limit", "{recorded}");
    assert!(
        recorded["message"]
            .as_str()
            .unwrap()
            .starts_with("started 3 times"),
        "{recorded}"
    );
    assert!(events(&db, "queue_service_down").is_empty());
}

/// A service of another build is replaced once, without counting it, and
/// the build that replacement runs is taken as it is: a supervisor whose
/// executable is already the next binary does not replace it again.
#[test]
fn a_service_of_another_build_is_replaced_once_and_then_accepted() {
    let (_fixture, repo, db) = idle_queue();
    let fake = std::sync::Arc::new(FakeService {
        running: std::sync::Mutex::new(Some("0.0.1-old".to_owned())),
        starts_as: Some("9.9.9-next".to_owned()),
        ..FakeService::default()
    });
    let supervisor = Running::start(&db, &repo, Path::new("/unused"), Some(fake.clone()));
    supervisor.passes(10);
    supervisor.drain();
    assert_eq!(fake.starts(), 1);
    let started = events(&db, "queue_service_started");
    assert_eq!(started.len(), 1, "{started:?}");
    assert_eq!(started[0]["payload"]["build"], "9.9.9-next");
    assert_eq!(started[0]["payload"]["replaced"]["build"], "0.0.1-old");
    assert_eq!(started[0]["payload"]["restart"], false);
    assert!(events(&db, "queue_service_down").is_empty());
}

/// Only when [`a_service_stops_once_the_test_process_that_started_it_is_gone`]
/// runs it: a test that starts its queue's service and then never returns,
/// timing out (`DAGQ_OWNER_PROBE_MODE=timeout`, the monitor's
/// `process::exit`) or waiting to be killed.
#[test]
#[ignore = "run by a_service_stops_once_the_test_process_that_started_it_is_gone"]
fn owner_probe() {
    let Some(dir) = std::env::var_os("DAGQ_OWNER_PROBE") else {
        return;
    };
    let db = PathBuf::from(dir).join("queue.db");
    drop(common::template::queue(&db));
    common::service::serve(&db);
    fs::write(db.with_file_name("served"), "").unwrap();
    let _waiting = (std::env::var("DAGQ_OWNER_PROBE_MODE").as_deref() == Ok("timeout"))
        .then(|| common::within(Duration::from_millis(300), "the probe's condition to hold"));
    loop {
        std::thread::park();
    }
}

/// A test process that ends without its drops, by a timeout's exit or a
/// SIGKILL, leaves its queue's directory, so the service it started does
/// not see its queue go; it stops within seconds of the process instead
/// (task 1352). The two ends run side by side.
#[test]
fn a_service_stops_once_the_test_process_that_started_it_is_gone() {
    use std::os::unix::process::ExitStatusExt;
    use std::process::{Command, Stdio};
    let waited = |what: &str, done: &mut dyn FnMut() -> bool| {
        let started = std::time::Instant::now();
        while !done() {
            assert!(started.elapsed() < Duration::from_secs(60), "{what}");
            std::thread::sleep(Duration::from_millis(100));
        }
    };
    let mut probes: Vec<_> = ["timeout", "kill"]
        .into_iter()
        .map(|mode| {
            let dir = tempfile::tempdir().unwrap();
            let probe = common::KillOnDrop::new(
                Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", "queue_service::owner_probe", "--ignored"])
                    .env("DAGQ_OWNER_PROBE", dir.path())
                    .env("DAGQ_OWNER_PROBE_MODE", mode)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .unwrap(),
                format!("the {mode} probe"),
            );
            (mode, dir, probe)
        })
        .collect();
    for (mode, dir, probe) in &mut probes {
        waited(
            &format!("the {mode} probe started the service"),
            &mut || dir.path().join("served").exists(),
        );
        // An owner that exited stays a zombie, there to the service, until
        // the wait below reaps it: the service runs until then.
        let running = service::probe(dir.path());
        assert_eq!(running.state.as_str(), "running", "{mode}: {running:?}");
        if *mode == "kill" {
            let pid = libc::pid_t::try_from(probe.child().id()).unwrap();
            // SAFETY: kill(2) takes no pointer.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
    for (mode, dir, probe) in &mut probes {
        let exit = {
            let _waiting = common::within(common::STEP_LIMIT, format!("the {mode} probe to end"));
            probe.child().wait().unwrap()
        };
        if *mode == "kill" {
            assert_eq!(exit.signal(), Some(libc::SIGKILL), "{exit:?}");
        } else {
            assert_eq!(exit.code(), Some(common::TIMED_OUT), "{exit:?}");
        }
        // No drop ran: the queue is there, and only its owner is gone.
        assert!(dir.path().join("queue.db").is_file(), "{mode}");
    }
    for (mode, dir, _) in &probes {
        waited(&format!("the {mode} probe's service stopped"), &mut || {
            service::probe(dir.path()).state.as_str() == "stopped"
        });
        // The service's last line says why it stopped and is written before
        // its lock is free, so its log has the reason once it looks stopped.
        let log = fs::read_to_string(service::log_path(dir.path())).unwrap();
        assert!(
            log.contains("the queue service stopped: owner_gone"),
            "{mode}: {:?}\n{log}",
            service::probe(dir.path())
        );
    }
}

/// A service of the version before this build's, which answers version 1
/// only: a look's `hello` and the supervisor's reach it and read its build
/// (so the supervisor replaces it rather than fail to start), and anything
/// else of this build's version it refuses before it does it.
#[test]
fn a_service_of_the_version_before_answers_the_hello_of_a_look_with_its_build() {
    use dagq::application::supervisor_service::ServiceCaller;
    use dagq::domain::queue_service::ServiceResponse;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    let dir = tempfile::tempdir().unwrap();
    let socket = service::socket_path(dir.path());
    std::fs::create_dir_all(service::service_dir(dir.path())).unwrap();
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let listener = UnixListener::bind(&socket).unwrap();
    let old = std::thread::spawn(move || {
        for _ in 0..3 {
            let (stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).unwrap();
            let request: ServiceRequest = serde_json::from_str(&line).unwrap();
            let mut response = if request.api_version == 1 {
                ServiceResponse::success(json!({"build": "0.0.1", "pid": 7}))
            } else {
                ServiceResponse::failure(ServiceErrorCode::ApiVersionMismatch, "not 1")
            };
            response.api_version = 1;
            response.min_api_version = 1;
            (&stream)
                .write_all(&serde_json::to_vec(&response).unwrap())
                .unwrap();
        }
    });
    let answered = service::hello(&socket, std::time::Duration::from_secs(5)).unwrap();
    assert_eq!(answered["build"], "0.0.1");
    let supervisor = service::SocketCaller::new(&dir.path().join("queue.db"));
    let answered = supervisor.call(UseCase::Hello, Value::Null).unwrap();
    assert_eq!(answered["build"], "0.0.1");
    let refused = supervisor
        .call(UseCase::Supervisor, json!({"op": "x"}))
        .unwrap_err();
    assert!(
        format!("{refused:#}").contains("API versions 1 to 1"),
        "{refused:#}"
    );
    let _waiting = common::within(common::STEP_LIMIT, "the old service to answer its calls");
    old.join().unwrap();
}

/// A read and a watch's look run on read-only connections, and a refusal
/// of either is recorded all the same, on a connection of its own; a look
/// the policy allows records nothing.
#[test]
fn a_refused_look_is_recorded_though_the_look_writes_nothing() {
    let queue = queue();
    start(&queue);
    let review = Principal::of(&ActorContext::review_job(queue.run.id(), 1));
    let review_token = token(&queue, &review);
    let refused = call(&queue, Some(&review_token), UseCase::WatchPoll, json!({}));
    assert_eq!(
        refused["error"]["code"], "authorization_denied",
        "{refused}"
    );
    let denials = events(&queue.db, "authorization_denied");
    assert_eq!(denials.len(), 1, "{denials:?}");
    assert_eq!(denials[0]["payload"]["capability"], "queue.watch");
    assert_eq!(denials[0]["actor"]["role"], "review-job");
    let looked = call(&queue, None, UseCase::WatchPoll, json!({}));
    assert_eq!(looked["ok"], true, "{looked}");
    let shown = call(
        &queue,
        Some(&review_token),
        UseCase::Show,
        json!({"id": queue.run.task_id().as_i64()}),
    );
    assert_eq!(shown["ok"], true, "{shown}");
    assert_eq!(events(&queue.db, "authorization_denied").len(), 1);
}
