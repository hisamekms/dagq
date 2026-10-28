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

/// The issue use case on the service's socket (ADR-t3102-1 decision 2):
/// the user (no token, a peer of the service's own OS user) is issued the
/// supervisor's token of a registration and the inbox's of a session, and
/// that supervisor a planner's and its wrapper's, each written by the
/// service, not by the caller. Refused as `authorization_denied`, recorded
/// and leaving no token: a parent not of the table (a worker for a wrapper,
/// the supervisor for the inbox, the user for a worker), the integrator's
/// asked for by anyone (the landing's use case issues it), and what is not
/// held in the issue's transaction (a run another supervisor holds the
/// lease of, a closed planner, a registration not in the queue or
/// expired). A principal named as another is a bad request.
#[test]
fn tokens_are_issued_by_the_service_at_the_request_of_their_parent() {
    use dagq::domain::{LeaseToken, PlannerOrigin, queue_service::SessionProcess};
    let queue = queue();
    start(&queue);
    let (lease, gone) = (LeaseToken::new("it-supervisor"), LeaseToken::new("gone"));
    let mut db = SqliteQueue::open(&queue.db).unwrap();
    db.register_supervisor(&lease, std::process::id(), 1, "test")
        .unwrap();
    let planner = db.open_planner(PlannerOrigin::Runtime, None).unwrap().id;
    let closed = db.open_planner(PlannerOrigin::Runtime, None).unwrap().id;
    db.close_planner(closed, None).unwrap();
    drop(db);
    let issue = |token: Option<&str>, principal: &Principal| {
        call(
            &queue,
            token,
            UseCase::IssueToken,
            json!({"principal": principal}),
        )
    };
    let issued_for = |answer: &Value| {
        assert_eq!(answer["ok"], true, "{answer}");
        let file: std::path::PathBuf =
            serde_json::from_value(answer["result"]["file"].clone()).unwrap();
        assert!(
            file.starts_with(service::service_dir(queue.dir())),
            "{file:?}"
        );
        let value = service::read_token(&file).unwrap();
        (
            service::lookup(queue.dir(), &value).unwrap().unwrap(),
            value,
        )
    };
    let supervisor = Principal::supervisor(&lease, std::process::id());
    let (principal, supervisor_token) = issued_for(&issue(None, &supervisor));
    assert_eq!(principal, supervisor);
    let inbox = Principal::inbox(SessionProcess {
        pid: std::process::id(),
        start: "now".into(),
    });
    assert_eq!(issued_for(&issue(None, &inbox)).0, inbox);
    let planner_target = |id: i64| {
        dagq::domain::session_step::StepTarget::of(
            dagq::domain::session_step::TargetKind::Planner,
            id,
        )
    };
    for child in [
        Principal::planner(planner.as_i64()),
        Principal::wrapper(&planner_target(planner.as_i64()), None),
    ] {
        assert_eq!(issued_for(&issue(Some(&supervisor_token), &child)).0, child);
    }

    let worker = Principal::worker(queue.run.id(), queue.run.task_id());
    let worker_token = token(&queue, &worker);
    // A supervisor whose registration is not in the queue.
    let gone_token = token(&queue, &Principal::supervisor(&gone, std::process::id()));
    let wrapper = Principal::wrapper(
        &dagq::domain::session_step::StepTarget::run(queue.run.id()),
        Some(queue.run.task_id()),
    );
    let integrator = Principal::integrator(queue.run.id(), &lease, std::process::id());
    let observer = Principal::of(&ActorContext::instance(
        dagq::domain::ActorRole::Observer,
        "s1",
    ));
    let not_held = "what it is issued under is not held";
    let refusals: Vec<(Option<&str>, &Principal, &str, Option<&str>)> = vec![
        (Some(worker_token.as_str()), &wrapper, "not granted", None),
        (
            Some(supervisor_token.as_str()),
            &inbox,
            "not on this resource",
            None,
        ),
        (None, &worker, "not on this resource", None),
        (None, &integrator, "not on this resource", None),
        (
            Some(supervisor_token.as_str()),
            &integrator,
            "not on this resource",
            None,
        ),
        // The run's lease is the claim's, not this supervisor's.
        (
            Some(supervisor_token.as_str()),
            &worker,
            not_held,
            Some("not leased"),
        ),
        (
            Some(gone_token.as_str()),
            &observer,
            not_held,
            Some("not registered"),
        ),
    ];
    let closed_planner = Principal::planner(closed.as_i64());
    let before = events(&queue.db, "authorization_denied").len();
    for (token, child, reason, why) in refusals.iter().copied().chain([(
        Some(supervisor_token.as_str()),
        &closed_planner,
        not_held,
        Some("not open"),
    )]) {
        let refused = issue(token, child);
        assert_eq!(
            refused["error"]["code"],
            json!(ServiceErrorCode::AuthorizationDenied.as_str()),
            "{child:?}: {refused}"
        );
        assert_eq!(refused["error"]["denied"]["reason"], reason, "{refused}");
        let last = events(&queue.db, "authorization_denied").pop().unwrap();
        assert_eq!(last["payload"]["capability"], "token.issue", "{last}");
        assert_eq!(
            last["payload"]["resource"],
            json!({"kind": "principal", "role": child.role.as_str()})
        );
        if let Some(why) = why {
            let recorded = last["payload"]["not_held"].as_str().unwrap();
            assert!(recorded.contains(why), "{recorded}");
        }
    }
    assert_eq!(
        events(&queue.db, "authorization_denied").len() - before,
        refusals.len() + 1
    );
    for child in [&wrapper, &integrator, &closed_planner] {
        assert!(
            !service::credential_path_of(queue.dir(), child).exists(),
            "{child:?}"
        );
    }
    // The registration's heartbeat expires: its supervisor is refused a
    // job's token, though its own sign-in (an exec's) is not.
    rusqlite::Connection::open(&queue.db)
        .unwrap()
        .execute(
            "UPDATE supervisors SET heartbeat_at = heartbeat_at - ?1 WHERE token = 'it-supervisor'",
            [dagq::domain::HEARTBEAT_TIMEOUT_SECS + 60],
        )
        .unwrap();
    let refused = issue(Some(&supervisor_token), &observer);
    assert_eq!(refused["error"]["denied"]["reason"], not_held, "{refused}");
    // A principal named as another is a bad request, and the other's token
    // stands.
    let mut named = observer.clone();
    named.actor_id = Principal::planner(planner.as_i64()).actor_id;
    let refused = issue(None, &named);
    assert_eq!(
        refused["error"]["code"],
        json!(ServiceErrorCode::BadRequest.as_str()),
        "{refused}"
    );
    assert!(
        service::credential_path_of(queue.dir(), &Principal::planner(planner.as_i64())).exists()
    );
}

/// A run's tokens under its lease (ADR-t3102-1 decision 2): the worker's,
/// its wrapper's and its executor's telemetry's are issued only to the
/// supervisor whose registration holds the run's lease and lives, both
/// found in the issue's transaction. Its registration expired or gone, or
/// another supervisor's lease, is refused as `authorization_denied`,
/// recorded with why, and leaves no token; once its heartbeat is written
/// again they are issued. No supervisor is issued another's sign-in.
#[test]
fn a_runs_tokens_are_issued_only_under_its_lease_by_a_live_registration() {
    use dagq::domain::LeaseToken;
    use dagq::domain::session_step::{StepTarget, TargetKind};
    let queue = queue();
    start(&queue);
    let (owner, other) = (LeaseToken::new("it-owner"), LeaseToken::new("it-other"));
    let mut db = SqliteQueue::open(&queue.db).unwrap();
    for lease in [&owner, &other] {
        db.register_supervisor(lease, std::process::id(), 1, "test")
            .unwrap();
    }
    drop(db);
    let conn = rusqlite::Connection::open(&queue.db).unwrap();
    conn.execute(
        "INSERT INTO run_leases(run_id,token,pid,heartbeat_at) VALUES (?1,?2,?3,unixepoch())
         ON CONFLICT(run_id) DO UPDATE SET token=excluded.token",
        rusqlite::params![queue.run.id().as_str(), owner.as_str(), std::process::id()],
    )
    .unwrap();
    let signed_in = |lease: &LeaseToken| {
        let answer = call(
            &queue,
            None,
            UseCase::IssueToken,
            json!({"principal": Principal::supervisor(lease, std::process::id())}),
        );
        let file: std::path::PathBuf =
            serde_json::from_value(answer["result"]["file"].clone()).unwrap();
        service::read_token(&file).unwrap()
    };
    let (owner_token, other_token) = (signed_in(&owner), signed_in(&other));
    let supervisor = |token: &str, op: &str, args: Value| {
        call(
            &queue,
            Some(token),
            UseCase::Supervisor,
            json!({"op": op, "args": args}),
        )
    };
    let worker = Principal::worker(queue.run.id(), queue.run.task_id());
    let wrapper = Principal::wrapper(&StepTarget::run(queue.run.id()), Some(queue.run.task_id()));
    let target = StepTarget::run(queue.run.id());
    let telemetry = Principal::telemetry(&target, Some(queue.run.task_id()));
    let issues = |token: &str, lease: &LeaseToken| {
        [
            supervisor(token, "issue_token", json!([worker])),
            supervisor(token, "issue_token", json!([wrapper])),
            supervisor(
                token,
                "issue_telemetry",
                json!([target, queue.run.task_id(), lease]),
            ),
        ]
    };
    let refused_all = |token: &str, lease: &LeaseToken, why: &str| {
        let before = events(&queue.db, "authorization_denied").len();
        for refused in issues(token, lease) {
            assert_eq!(
                refused["error"]["code"], "authorization_denied",
                "{refused}"
            );
        }
        let denials = events(&queue.db, "authorization_denied");
        assert_eq!(denials.len() - before, 3);
        for denial in &denials[before..] {
            assert_eq!(denial["payload"]["capability"], "token.issue", "{denial}");
            let recorded = denial["payload"]["not_held"].as_str().unwrap();
            assert!(recorded.contains(why), "{recorded}");
        }
        for child in [&worker, &wrapper, &telemetry] {
            assert!(
                !service::credential_path_of(queue.dir(), child).exists(),
                "{child:?}"
            );
        }
    };
    // Another supervisor's lease.
    refused_all(&other_token, &other, "not leased");
    // Its lease named by another supervisor: refused before the issue, and
    // recorded.
    let before = events(&queue.db, "authorization_denied").len();
    let named = supervisor(
        &other_token,
        "issue_telemetry",
        json!([target, queue.run.task_id(), owner]),
    );
    assert_eq!(named["error"]["code"], "authorization_denied", "{named}");
    let denials = events(&queue.db, "authorization_denied");
    assert_eq!(denials.len() - before, 1);
    assert!(
        denials[denials.len() - 1]["payload"]["not_held"]
            .as_str()
            .unwrap()
            .contains("not the supervisor principal's registration"),
        "{named}"
    );
    assert!(!service::credential_path_of(queue.dir(), &telemetry).exists());
    // The owner's registration expired, then gone.
    let heartbeat = |at: &str| {
        conn.execute(
            &format!("UPDATE supervisors SET heartbeat_at = {at} WHERE token = ?1"),
            [owner.as_str()],
        )
        .unwrap();
    };
    heartbeat(&format!(
        "unixepoch() - {}",
        dagq::domain::HEARTBEAT_TIMEOUT_SECS + 60
    ));
    refused_all(&owner_token, &owner, "expired");
    // The executor's telemetry of a target of no run, under the registration
    // it names: refused while it is expired, issued once it lives.
    let planner = StepTarget::of(TargetKind::Planner, 1);
    let planners = || {
        supervisor(
            &owner_token,
            "issue_telemetry",
            json!([planner, null, owner]),
        )
    };
    let expired = planners();
    assert_eq!(
        expired["error"]["code"], "authorization_denied",
        "{expired}"
    );
    let denial = events(&queue.db, "authorization_denied").pop().unwrap();
    assert!(
        denial["payload"]["not_held"]
            .as_str()
            .unwrap()
            .contains("expired"),
        "{denial}"
    );
    heartbeat("unixepoch()");
    for issued in issues(&owner_token, &owner) {
        assert_eq!(issued["ok"], true, "{issued}");
    }
    let issued = planners();
    assert_eq!(issued["ok"], true, "{issued}");
    for child in [&worker, &wrapper, &telemetry] {
        let file = service::credential_path_of(queue.dir(), child);
        let value = service::read_token(&file).unwrap();
        assert_eq!(
            service::lookup(queue.dir(), &value).unwrap().as_ref(),
            Some(child)
        );
    }
    // A run's executor's token is never issued under no lease.
    let before = events(&queue.db, "authorization_denied").len();
    let leaseless = supervisor(
        &owner_token,
        "issue_telemetry",
        json!([target, queue.run.task_id(), null]),
    );
    assert_eq!(
        leaseless["error"]["code"], "authorization_denied",
        "{leaseless}"
    );
    assert_eq!(events(&queue.db, "authorization_denied").len() - before, 1);
    // Nor a sign-in of another's registration, which would write under its
    // leases.
    let another = supervisor(
        &owner_token,
        "issue_token",
        json!([Principal::supervisor(&other, std::process::id())]),
    );
    assert_eq!(
        another["error"]["code"], "authorization_denied",
        "{another}"
    );
    conn.execute("DELETE FROM supervisors WHERE token = ?1", [owner.as_str()])
        .unwrap();
    let gone = supervisor(&owner_token, "issue_token", json!([worker]));
    assert_eq!(gone["error"]["code"], "authorization_denied", "{gone}");
    let denial = events(&queue.db, "authorization_denied").pop().unwrap();
    assert!(
        denial["payload"]["not_held"]
            .as_str()
            .unwrap()
            .contains("not registered"),
        "{denial}"
    );
}

/// The executor's telemetry of a job a command starts (`observe`,
/// `throughput-review`; ADR-t3102-1 decision 2): its token is issued by the
/// service, under its own lock, at the request of the command's caller,
/// the user (no token) or the supervisor (its registration live), while its
/// target is open and holds no token yet. Refused as
/// `authorization_denied`, recorded and leaving no token: a target that
/// ended, one of a run or of a job no command starts (the user's), a
/// caller that starts no executor (a worker), a target another caller's
/// command started, and a target whose executor holds its token already
/// (its job's, which the issue would revoke).
#[test]
fn a_commands_job_telemetry_token_is_issued_at_its_callers_request() {
    use dagq::domain::session_step::{StepTarget, TargetKind};
    use dagq::domain::{EventKind, LeaseToken};
    let queue = queue();
    start(&queue);
    let lease = LeaseToken::new("it-observing");
    let pid = std::process::id();
    let mut db = SqliteQueue::open(&queue.db).unwrap();
    db.register_supervisor(&lease, pid, 1, "test").unwrap();
    // Each started by a command of this process, as its caller.
    let observation = |db: SqliteQueue, by: ActorContext, dir: &str| {
        let db = db.with_actor(by);
        let id = db
            .record_queue_event(
                EventKind::ObserveStarted,
                json!({"dir": dir, "timeout_secs": 600, "pid": pid}),
            )
            .unwrap();
        (db, StepTarget::of(TargetKind::Observer, id.as_i64()))
    };
    let (db, open) = observation(db, ActorContext::user(), "/o/1");
    let (db, by_supervisor) = observation(
        db,
        ActorContext::instance(dagq::domain::ActorRole::Supervisor, pid),
        "/o/2",
    );
    let (db, ended) = observation(db, ActorContext::user(), "/o/3");
    db.record_queue_event(EventKind::ObserveFinished, json!({"dir": "/o/3"}))
        .unwrap();
    drop(db);
    let issue = |token: Option<&str>, principal: &Principal| {
        call(
            &queue,
            token,
            UseCase::IssueToken,
            json!({"principal": principal}),
        )
    };
    let issued = |answer: &Value| {
        assert_eq!(answer["ok"], true, "{answer}");
        let file: std::path::PathBuf =
            serde_json::from_value(answer["result"]["file"].clone()).unwrap();
        assert!(
            file.starts_with(service::service_dir(queue.dir())),
            "{file:?}"
        );
        service::lookup(queue.dir(), &service::read_token(&file).unwrap())
            .unwrap()
            .unwrap()
    };
    let telemetry = |target: &StepTarget| Principal::telemetry(target, None);
    // The user's command's.
    assert_eq!(issued(&issue(None, &telemetry(&open))), telemetry(&open));
    // The supervisor's command's.
    let supervisor = token(&queue, &Principal::supervisor(&lease, std::process::id()));
    assert_eq!(
        issued(&issue(Some(&supervisor), &telemetry(&by_supervisor))),
        telemetry(&by_supervisor)
    );

    let worker = token(
        &queue,
        &Principal::worker(queue.run.id(), queue.run.task_id()),
    );
    let of_run = Principal::telemetry(&StepTarget::run(queue.run.id()), Some(queue.run.task_id()));
    let plan_review = telemetry(&StepTarget::of(TargetKind::PlanReview, 1));
    let refusals: Vec<(Option<&str>, Principal, &str, &str)> = vec![
        (
            None,
            telemetry(&ended),
            "what it is issued under is not held",
            "has ended",
        ),
        (None, of_run, "not on this resource", "lease"),
        (
            None,
            plan_review,
            "not on this resource",
            "not of a job a command starts",
        ),
        (Some(worker.as_str()), telemetry(&open), "not granted", ""),
        // A command of another caller's started it.
        (
            None,
            telemetry(&by_supervisor),
            "not on this resource",
            "started by another caller",
        ),
        // The job's executor holds it: its own caller asking again does
        // not take it over.
        (
            Some(supervisor.as_str()),
            telemetry(&by_supervisor),
            "not on this resource",
            "holds its token already",
        ),
    ];
    for (token, child, reason, why) in &refusals {
        let before = events(&queue.db, "authorization_denied").len();
        let refused = issue(*token, child);
        assert_eq!(
            refused["error"]["code"],
            json!(ServiceErrorCode::AuthorizationDenied.as_str()),
            "{child:?}: {refused}"
        );
        assert_eq!(refused["error"]["denied"]["reason"], *reason, "{refused}");
        let denied = events(&queue.db, "authorization_denied");
        assert_eq!(denied.len() - before, 1, "{child:?}");
        let last = &denied[denied.len() - 1]["payload"];
        assert_eq!(last["capability"], "token.issue", "{last}");
        assert_eq!(
            last["resource"],
            json!({"kind": "principal", "role": "executor-telemetry"})
        );
        let recorded = last["not_held"]
            .as_str()
            .or(last["why"].as_str())
            .unwrap_or("");
        assert!(recorded.contains(why), "{recorded}");
    }
    assert!(
        dagq::application::queue_service::TelemetryTokens::telemetry_credential(
            &dagq::infrastructure::queue_service::SystemServiceAccess,
            &queue.db,
            &ended,
        )
        .is_none()
    );
}

/// Two requests at once of the caller whose command started a job for
/// the token of that job's executor, which holds none yet (a caller that
/// did not start it is refused before): the service looks at what the target
/// holds in the transaction that issues it, so one of them is issued and
/// the other is refused as `authorization_denied` and recorded, and the
/// token issued is the one that holds (no later issue revoked it).
#[test]
fn of_two_callers_asking_at_once_for_a_jobs_executor_token_one_is_issued() {
    use dagq::domain::EventKind;
    use dagq::domain::session_step::{StepTarget, TargetKind};
    let queue = queue();
    start(&queue);
    let db = SqliteQueue::open(&queue.db)
        .unwrap()
        .with_actor(ActorContext::user());
    for round in 0..5 {
        let id = db
            .record_queue_event(
                EventKind::ObserveStarted,
                json!({"dir": format!("/o/{round}"), "timeout_secs": 600, "pid": std::process::id()}),
            )
            .unwrap();
        let telemetry =
            Principal::telemetry(&StepTarget::of(TargetKind::Observer, id.as_i64()), None);
        let before = events(&queue.db, "authorization_denied").len();
        let barrier = std::sync::Barrier::new(2);
        // The barrier and the joins end within the limit however a caller
        // ends, one that never reaches the barrier included.
        let _waiting = common::within(
            common::STEP_LIMIT,
            "the two callers asking at once to be answered",
        );
        let answers: Vec<Value> = std::thread::scope(|scope| {
            let asks: Vec<_> = (0..2)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        call(
                            &queue,
                            None,
                            UseCase::IssueToken,
                            json!({"principal": telemetry}),
                        )
                    })
                })
                .collect();
            asks.into_iter().map(|ask| ask.join().unwrap()).collect()
        });
        let issued: Vec<&Value> = answers
            .iter()
            .filter(|answer| answer["ok"] == true)
            .collect();
        assert_eq!(issued.len(), 1, "round {round}: {answers:?}");
        let refused = answers.iter().find(|answer| answer["ok"] != true).unwrap();
        assert_eq!(
            refused["error"]["code"], "authorization_denied",
            "{refused}"
        );
        let denied = events(&queue.db, "authorization_denied");
        assert_eq!(denied.len() - before, 1, "round {round}");
        assert!(
            denied[denied.len() - 1]["payload"]["why"]
                .as_str()
                .unwrap()
                .contains("holds its token already")
        );
        let file: std::path::PathBuf =
            serde_json::from_value(issued[0]["result"]["file"].clone()).unwrap();
        assert_eq!(
            service::lookup(queue.dir(), &service::read_token(&file).unwrap())
                .unwrap()
                .as_ref(),
            Some(&telemetry),
            "round {round}"
        );
    }
}

/// The landing slot and the integrator's token (ADR-t3102-1 decision 2):
/// the supervisor that holds a run's lease takes its slot through the
/// service's `begin_landing`, which issues the integrator's token in the
/// same transaction, under the lease and its registration live. A
/// supervisor whose run another holds the lease of, that names a lease not
/// its registration, or whose registration expired or is not in the queue,
/// is refused as `authorization_denied`, recorded, and takes no slot and
/// leaves no token. The token is kept with the supervisor that asked, the
/// generic issue use case issues it to no one, and `end_landing` revokes
/// it.
#[test]
fn the_landing_slot_issues_the_integrators_token_under_its_lease() {
    use dagq::domain::{LeaseToken, RunStatus};
    let queue = queue();
    start(&queue);
    let (lease, other) = (LeaseToken::new("it-lander"), LeaseToken::new("it-other"));
    let mut db = SqliteQueue::open(&queue.db).unwrap();
    for token in [&lease, &other] {
        db.register_supervisor(token, std::process::id(), 1, "test")
            .unwrap();
    }
    drop(db);
    let conn = rusqlite::Connection::open(&queue.db).unwrap();
    conn.execute(
        "UPDATE task_runs SET status='awaiting_integration' WHERE id=?1",
        [queue.run.id().as_str()],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO run_leases(run_id,token,pid,heartbeat_at) VALUES (?1,?2,?3,unixepoch())
         ON CONFLICT(run_id) DO UPDATE SET token=excluded.token",
        rusqlite::params![queue.run.id().as_str(), lease.as_str(), std::process::id()],
    )
    .unwrap();
    let sign_in = |lease: &LeaseToken| {
        let answer = call(
            &queue,
            None,
            UseCase::IssueToken,
            json!({"principal": Principal::supervisor(lease, std::process::id())}),
        );
        let file: std::path::PathBuf =
            serde_json::from_value(answer["result"]["file"].clone()).unwrap();
        service::read_token(&file).unwrap()
    };
    let supervisor = |token: &str, op: &str, args: Value| {
        call(
            &queue,
            Some(token),
            UseCase::Supervisor,
            json!({"op": op, "args": args}),
        )
    };
    let main = "a".repeat(40);
    let integrator = Principal::integrator(queue.run.id(), &lease, std::process::id());
    let file = service::credential_path_of(queue.dir(), &integrator);
    let begin = |token: &str, lease: &LeaseToken| {
        supervisor(
            token,
            "begin_landing",
            json!([queue.run.id(), lease, main, false]),
        )
    };

    let denied = || events(&queue.db, "authorization_denied").len();
    let last_denial = || events(&queue.db, "authorization_denied").pop().unwrap();
    let status = |queue: &crate::common::service::Queue| {
        SqliteQueue::open(&queue.db)
            .unwrap()
            .run(queue.run.id())
            .unwrap()
            .status()
    };

    let other_token = sign_in(&other);
    // The run's lease is another supervisor's: found in the slot's
    // transaction, refused and recorded.
    let before = denied();
    let refused = begin(&other_token, &other);
    assert_eq!(
        refused["error"]["code"], "authorization_denied",
        "{refused}"
    );
    assert_eq!(denied() - before, 1);
    let denial = last_denial();
    assert_eq!(denial["payload"]["capability"], "token.issue", "{denial}");
    assert_eq!(
        denial["payload"]["resource"],
        json!({"kind": "principal", "role": "integrator"})
    );
    assert!(
        denial["payload"]["not_held"]
            .as_str()
            .unwrap()
            .contains("still leased"),
        "{denial}"
    );
    assert!(!file.exists());
    // Another's lease named: refused before the slot, and recorded.
    let before = denied();
    let named = begin(&other_token, &lease);
    assert_eq!(named["error"]["code"], "authorization_denied", "{named}");
    assert_eq!(denied() - before, 1);
    let denial = last_denial();
    assert_eq!(
        denial["payload"]["capability"], "landing.request",
        "{denial}"
    );
    assert!(
        denial["payload"]["not_held"]
            .as_str()
            .unwrap()
            .contains("not the supervisor principal's registration"),
        "{denial}"
    );
    assert!(!file.exists());
    assert_eq!(status(&queue), RunStatus::AwaitingIntegration);

    let lander = sign_in(&lease);
    // Its registration expired, or not in the queue: no slot and no token,
    // the refusal recorded.
    let gone = LeaseToken::new("it-gone");
    let gone_token = sign_in(&gone);
    conn.execute(
        "UPDATE supervisors SET heartbeat_at = heartbeat_at - ?2 WHERE token = ?1",
        rusqlite::params![lease.as_str(), dagq::domain::HEARTBEAT_TIMEOUT_SECS + 60],
    )
    .unwrap();
    for (token, lease, why) in [
        (&lander, &lease, "expired"),
        (&gone_token, &gone, "not registered"),
    ] {
        let before = denied();
        let refused = begin(token, lease);
        assert_eq!(
            refused["error"]["code"], "authorization_denied",
            "{refused}"
        );
        assert_eq!(denied() - before, 1);
        let denial = last_denial();
        assert_eq!(
            denial["payload"]["resource"],
            json!({"kind": "principal", "role": "integrator"})
        );
        assert!(
            denial["payload"]["not_held"]
                .as_str()
                .unwrap()
                .contains(why),
            "{denial}"
        );
        assert!(!file.exists());
        assert_eq!(status(&queue), RunStatus::AwaitingIntegration);
    }
    conn.execute(
        "UPDATE supervisors SET heartbeat_at = unixepoch() WHERE token = ?1",
        [lease.as_str()],
    )
    .unwrap();
    drop(conn);
    let begun = begin(&lander, &lease);
    assert_eq!(begun["ok"], true, "{begun}");
    assert_eq!(status(&queue), RunStatus::Integrating);
    let issued: std::path::PathBuf =
        serde_json::from_value(begun["result"]["integrator"].clone()).unwrap();
    assert_eq!(issued, file);
    let token = service::read_token(&file).unwrap();
    let started = events(&queue.db, "integration_started").pop().unwrap();
    assert_eq!(
        service::lookup(queue.dir(), &token).unwrap(),
        Some(
            integrator
                .clone()
                .of_landing(dagq::domain::EventId::new(started["id"].as_i64().unwrap()))
        )
    );
    // Kept with the supervisor that asked.
    let kept = dagq::infrastructure::queue_service::held(queue.dir(), &integrator.credential_key())
        .unwrap()
        .unwrap();
    assert_eq!(
        kept.issued_by.map(|issuer| issuer.parent),
        Some(Principal::supervisor(&lease, std::process::id()).credential_key())
    );
    // No requester is issued it by the generic use case.
    let generic = call(
        &queue,
        Some(&lander),
        UseCase::IssueToken,
        json!({"principal": integrator}),
    );
    assert_eq!(
        generic["error"]["code"], "authorization_denied",
        "{generic}"
    );
    // The landing's end revokes it.
    let ended = supervisor(&lander, "end_landing", json!([queue.run.id()]));
    assert_eq!(ended["result"], true, "{ended}");
    assert!(!file.exists());
    assert_eq!(service::lookup(queue.dir(), &token).unwrap(), None);
}

/// A job's token (ADR-t3102-1 decision 2) is replaced only at the request
/// of the parent that asked for it, once the process that asked, which
/// the job ran under, has ended. The inbox, the user and another
/// supervisor asking for the token of a job a supervisor started, whose
/// process runs, are refused as `authorization_denied`, recorded, and the
/// job's token still holds; the other supervisor may not revoke it
/// either. Its own supervisor asking again is handed the same token. Once
/// that process ended, another parent is still refused, its supervisor
/// replaces it, and the sweep revokes a left one but never a running
/// job's.
#[test]
fn the_inbox_cannot_take_the_token_of_a_job_another_parent_started() {
    use dagq::domain::LeaseToken;
    use dagq::domain::queue_service::{SessionProcess, TokenIssuer};
    let queue = queue();
    start(&queue);
    let pid = std::process::id();
    let (lease, other) = (LeaseToken::new("it-jobs"), LeaseToken::new("it-other"));
    let mut db = SqliteQueue::open(&queue.db).unwrap();
    for lease in [&lease, &other] {
        db.register_supervisor(lease, pid, 1, "test").unwrap();
    }
    drop(db);
    let sign_in = |lease: &LeaseToken| {
        let answer = call(
            &queue,
            None,
            UseCase::IssueToken,
            json!({"principal": Principal::supervisor(lease, pid)}),
        );
        let file: std::path::PathBuf =
            serde_json::from_value(answer["result"]["file"].clone()).unwrap();
        service::read_token(&file).unwrap()
    };
    let (supervisor, another) = (sign_in(&lease), sign_in(&other));
    let inbox = token(
        &queue,
        &Principal::inbox(SessionProcess {
            pid,
            start: dagq::infrastructure::adapters::session_start(pid).unwrap(),
        }),
    );
    let op = |token: &str, op: &str, args: Value| {
        call(
            &queue,
            Some(token),
            UseCase::Supervisor,
            json!({"op": op, "args": args}),
        )
    };
    let asked = |token: Option<&str>, job: &Principal| {
        call(
            &queue,
            token,
            UseCase::IssueToken,
            json!({"principal": job}),
        )
    };
    let value_of = |job: &Principal| {
        service::read_token(&service::credential_path_of(queue.dir(), job)).unwrap()
    };
    let refused = |answer: &Value, why: &str| {
        assert_eq!(answer["error"]["code"], "authorization_denied", "{answer}");
        let denial = events(&queue.db, "authorization_denied").pop().unwrap();
        assert_eq!(denial["payload"]["capability"], "token.issue", "{denial}");
        assert!(
            denial["payload"]["why"].as_str().unwrap().contains(why),
            "{denial}"
        );
    };

    // The supervisor's job, its process (this one, which asked) running.
    let job = Principal::of(&ActorContext::throughput_review_job("hourly", "it"));
    let first = op(&supervisor, "issue_token", json!([job]));
    assert_eq!(first["ok"], true, "{first}");
    assert_eq!(first["result"]["held"], false, "{first}");
    let value = value_of(&job);
    for who in [
        "inbox",
        "user",
        "another supervisor",
        "another supervisor's revocation",
    ] {
        let before = events(&queue.db, "authorization_denied").len();
        let answer = match who {
            "inbox" => asked(Some(&inbox), &job),
            "user" => asked(None, &job),
            "another supervisor" => op(&another, "issue_token", json!([job])),
            _ => op(&another, "revoke_token", json!([job.actor_id])),
        };
        refused(&answer, "another parent's");
        assert_eq!(
            events(&queue.db, "authorization_denied").len() - before,
            1,
            "{who}"
        );
        assert_eq!(value_of(&job), value, "{who}");
        assert_eq!(
            service::lookup(queue.dir(), &value).unwrap(),
            Some(job.clone()),
            "{who}"
        );
    }
    // Its own supervisor is handed the same token again, not a new one,
    // and told so: a start it was handed to gives back nothing.
    let again = op(&supervisor, "issue_token", json!([job]));
    assert_eq!(again["ok"], true, "{again}");
    assert_eq!(again["result"]["held"], true, "{again}");
    assert_eq!(again["result"]["file"], first["result"]["file"], "{again}");
    assert_eq!(value_of(&job), value);
    // Not swept while its process runs.
    assert_eq!(
        op(&supervisor, "revoke_principal", json!([job]))["result"],
        false
    );
    assert_eq!(
        service::lookup(queue.dir(), &value).unwrap(),
        Some(job.clone())
    );

    // A job whose process that asked has ended: its parent alone replaces
    // it, and a left one is swept.
    let gone = TokenIssuer::of(
        &Principal::supervisor(&lease, pid),
        Some(SessionProcess {
            pid,
            start: "a process ended".into(),
        }),
    );
    let left = Principal::of(&ActorContext::throughput_review_job("hourly", "left"));
    service::issue_by(queue.dir(), &left, &gone, 1).unwrap();
    let old = value_of(&left);
    refused(&asked(Some(&inbox), &left), "another parent's");
    assert_eq!(
        service::lookup(queue.dir(), &old).unwrap(),
        Some(left.clone())
    );
    assert_eq!(op(&supervisor, "issue_token", json!([left]))["ok"], true);
    assert_eq!(service::lookup(queue.dir(), &old).unwrap(), None);
    let swept = Principal::of(&ActorContext::throughput_review_job("hourly", "swept"));
    service::issue_by(queue.dir(), &swept, &gone, 1).unwrap();
    assert_eq!(
        op(&another, "revoke_principal", json!([swept]))["result"],
        true
    );
    assert!(!service::credential_path_of(queue.dir(), &swept).exists());
}

/// A job's token whose parent cannot be judged (ADR-t3102-1 decision 2): one
/// an older binary left with no parent kept, as a throughput review whose
/// command fell before its finish leaves, is refused over while the queue
/// tells that job may still run (its command's process runs), recorded,
/// and neither replaced, revoked on request nor swept. Once that process
/// is gone, the period's review started again (its own start recorded
/// first, under its own process) is issued a new token in its place, and
/// the sweep takes a left one. A run's job (a recovery job) is told by its
/// `headless_jobs` row; one the inbox asked for from a process the service
/// could not read is swept once its job's finish is recorded; a
/// supervisor's whose process was not read, once that supervisor's
/// registration went.
#[test]
fn a_job_token_an_older_binary_left_goes_once_the_queue_tells_its_job_ended() {
    use dagq::application::{HeadlessJobStore, NewHeadlessJob};
    use dagq::domain::{
        EventKind, LeaseToken, RunId,
        queue_service::{SessionProcess, TokenIssuer},
    };
    let queue = queue();
    start(&queue);
    let pid = std::process::id();
    let (lease, other) = (LeaseToken::new("it-runs"), LeaseToken::new("it-other"));
    let mut db = SqliteQueue::open(&queue.db).unwrap();
    for lease in [&lease, &other] {
        db.register_supervisor(lease, pid, 1, "test").unwrap();
    }
    let db = db;
    let supervisor = token(&queue, &Principal::supervisor(&lease, pid));
    let op = |op: &str, args: Value| {
        call(
            &queue,
            Some(&supervisor),
            UseCase::Supervisor,
            json!({"op": op, "args": args}),
        )
    };
    let value_of = |job: &Principal| {
        service::read_token(&service::credential_path_of(queue.dir(), job)).unwrap()
    };
    let refused = |answer: &Value| {
        assert_eq!(answer["error"]["code"], "authorization_denied", "{answer}");
        let denial = events(&queue.db, "authorization_denied").pop().unwrap();
        assert!(
            denial["payload"]["why"]
                .as_str()
                .unwrap()
                .contains("no parent that can be judged"),
            "{denial}"
        );
    };
    // The start of a review of `period` by the command of process `pid`.
    let started = |period: &str, pid: u32| {
        db.record_queue_event(
            EventKind::ThroughputReviewStarted,
            json!({"mode": "hourly", "period": period, "dir": format!("/reviews/{period}/{pid}"),
                   "pid": pid, "timeout_secs": 3600}),
        )
        .unwrap();
    };
    let job = |period: &str| Principal::of(&ActorContext::throughput_review_job("hourly", period));

    // An older binary's review of period 10, whose command still runs.
    let mut command = common::KillOnDrop::new(
        std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .unwrap(),
        "the older binary's review command",
    );
    let gone = command.child().id();
    let ten = job("10");
    service::issue(queue.dir(), &ten, 1).unwrap();
    started("10", gone);
    let left = value_of(&ten);
    let before = events(&queue.db, "authorization_denied").len();
    refused(&op("issue_token", json!([ten])));
    assert_eq!(events(&queue.db, "authorization_denied").len() - before, 1);
    // Nor revoked on request, which would let the issue take it over.
    refused(&op("revoke_token", json!([ten.actor_id])));
    assert_eq!(events(&queue.db, "authorization_denied").len() - before, 2);
    assert_eq!(op("revoke_principal", json!([ten]))["result"], false);
    assert_eq!(value_of(&ten), left);
    refused(&op("issue_token", json!([ten])));
    assert_eq!(value_of(&ten), left);
    // Its command is gone with no finish; the period is reviewed again,
    // whose start, recorded first, names the process that asks.
    drop(command);
    started("10", pid);
    let issued = op("issue_token", json!([ten]));
    assert_eq!(issued["ok"], true, "{issued}");
    assert_ne!(value_of(&ten), left);
    assert_eq!(service::lookup(queue.dir(), &left).unwrap(), None);
    // One left by a command gone is swept.
    let eleven = job("11");
    service::issue(queue.dir(), &eleven, 1).unwrap();
    started("11", gone);
    let value = value_of(&eleven);
    assert_eq!(op("revoke_principal", json!([eleven]))["result"], true);
    assert_eq!(service::lookup(queue.dir(), &value).unwrap(), None);

    // A run's recovery job, by its row, which a live supervisor runs.
    let run = RunId::new("it-run").unwrap();
    let recovery = Principal::of(&ActorContext::recovery_job(&run, "idle", 1));
    service::issue(queue.dir(), &recovery, 1).unwrap();
    let row = db
        .record_headless_job(&NewHeadlessJob {
            kind: dagq::domain::headless_job::RECOVERY,
            label: Some("idle".into()),
            run_id: Some(run.clone()),
            proposal_id: None,
            goal_id: None,
            attempt: 1,
            provider: None,
            pid,
            process_start: None,
            supervisor_token: other.clone(),
            stdout: None,
        })
        .unwrap();
    refused(&op("issue_token", json!([recovery])));
    db.end_headless_job(row, "ended").unwrap();
    assert_eq!(op("issue_token", json!([recovery]))["ok"], true);

    // The inbox's, asked for from a process the service could not read.
    let unread = job("12");
    let inbox = Principal::inbox(SessionProcess {
        pid,
        start: dagq::infrastructure::adapters::session_start(pid).unwrap(),
    });
    service::issue_by(queue.dir(), &unread, &TokenIssuer::of(&inbox, None), 1).unwrap();
    started("12", pid);
    assert_eq!(op("revoke_principal", json!([unread]))["result"], false);
    db.record_queue_event(
        EventKind::ThroughputReviewFinished,
        json!({"mode": "hourly", "period": "12", "dir": format!("/reviews/12/{pid}")}),
    )
    .unwrap();
    assert_eq!(op("revoke_principal", json!([unread]))["result"], true);

    // A supervisor's whose process was not read, by its registration.
    let registered = job("13");
    service::issue_by(
        queue.dir(),
        &registered,
        &TokenIssuer::of(&Principal::supervisor(&other, pid), None),
        1,
    )
    .unwrap();
    assert_eq!(op("revoke_principal", json!([registered]))["result"], false);
    db.deregister_supervisor(&other).unwrap();
    assert_eq!(op("revoke_principal", json!([registered]))["result"], true);
}

/// A job's token an older binary left with no parent kept, whose job the
/// queue keeps no record of (ADR-t3102-1 decision 2): a recovery job's
/// whose supervisor stopped after its token and before its `headless_jobs`
/// row. While the supervisor registered by its issue runs, another
/// supervisor's issue over it and its revocation are refused and recorded,
/// it is not swept, and it holds. Once that process ended, the same
/// actor's start is issued a new token in its place, and one left so is
/// swept. The one supervisor that alone may have asked for one is handed
/// it again. A setup review's, of which the queue keeps no record, goes
/// once no `dagq setup-review` that started by its issue runs, whatever
/// supervisor runs.
#[test]
fn a_job_token_an_older_binary_left_with_no_record_goes_once_no_process_that_may_have_asked_runs() {
    use dagq::domain::{LeaseToken, RunId};
    let queue = queue();
    start(&queue);
    let pid = std::process::id();
    let now = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap();
    let mut stopped = common::KillOnDrop::new(
        std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .unwrap(),
        "the process of the supervisor that asked",
    );
    let (old, new) = (LeaseToken::new("it-old"), LeaseToken::new("it-new"));
    let mut db = SqliteQueue::open(&queue.db).unwrap();
    db.register_supervisor(&old, stopped.child().id(), 1, "test")
        .unwrap();
    db.register_supervisor(&new, pid, 1, "test").unwrap();
    drop(db);
    // The old one registered before the tokens, the new one after.
    rusqlite::Connection::open(&queue.db)
        .unwrap()
        .execute(
            "UPDATE supervisors SET started_at = ?1 WHERE token = ?2",
            rusqlite::params![now - 200, old.as_str()],
        )
        .unwrap();
    let supervisor = token(&queue, &Principal::supervisor(&new, pid));
    let op = |op: &str, args: Value| {
        call(
            &queue,
            Some(&supervisor),
            UseCase::Supervisor,
            json!({"op": op, "args": args}),
        )
    };
    let value_of = |job: &Principal| {
        service::read_token(&service::credential_path_of(queue.dir(), job)).unwrap()
    };
    let denials = || events(&queue.db, "authorization_denied");
    let refused = |answer: &Value| {
        assert_eq!(answer["error"]["code"], "authorization_denied", "{answer}");
        let denial = denials().pop().unwrap();
        assert!(
            denial["payload"]["why"]
                .as_str()
                .unwrap()
                .contains("no record of its job"),
            "{denial}"
        );
    };
    let run = RunId::new("it-run").unwrap();
    let job = |attempt| Principal::of(&ActorContext::recovery_job(&run, "idle", attempt));
    let (first, second) = (job(1), job(2));
    for job in [&first, &second] {
        service::issue(queue.dir(), job, now - 100).unwrap();
    }
    let left = value_of(&first);
    let before = denials().len();
    refused(&op("issue_token", json!([first])));
    refused(&op("revoke_token", json!([first.actor_id])));
    assert_eq!(denials().len() - before, 2);
    assert_eq!(op("revoke_principal", json!([second]))["result"], false);
    assert_eq!(value_of(&first), left);
    assert!(service::credential_path_of(queue.dir(), &second).exists());

    // The process of the supervisor that asked ended.
    drop(stopped);
    let issued = op("issue_token", json!([first]));
    assert_eq!(issued["ok"], true, "{issued}");
    assert_ne!(value_of(&first), left);
    assert_eq!(service::lookup(queue.dir(), &left).unwrap(), None);
    assert_eq!(op("revoke_principal", json!([second]))["result"], true);
    assert!(!service::credential_path_of(queue.dir(), &second).exists());

    // One issued since the new one registered, which alone may have asked
    // for it: handed the same token again.
    let third = job(3);
    service::issue(queue.dir(), &third, now + 100).unwrap();
    let held = value_of(&third);
    let again = op("issue_token", json!([third]));
    assert_eq!(again["ok"], true, "{again}");
    assert_eq!(value_of(&third), held);

    // A setup review's, issued before any `dagq` that runs started, which
    // a person starts again. A live supervisor registered before it does
    // not keep it: only its command starts a setup review.
    let ancient = LeaseToken::new("it-ancient");
    SqliteQueue::open(&queue.db)
        .unwrap()
        .register_supervisor(&ancient, pid, 1, "test")
        .unwrap();
    rusqlite::Connection::open(&queue.db)
        .unwrap()
        .execute(
            "UPDATE supervisors SET started_at = 0 WHERE token = ?1",
            rusqlite::params![ancient.as_str()],
        )
        .unwrap();
    let setup = Principal::of(&ActorContext::setup_review_job("it-place"));
    service::issue(queue.dir(), &setup, 1).unwrap();
    let left = value_of(&setup);
    let issued = call(
        &queue,
        None,
        UseCase::IssueToken,
        json!({"principal": setup}),
    );
    assert_eq!(issued["ok"], true, "{issued}");
    assert_ne!(value_of(&setup), left);
}

/// The executor's telemetry of a target of no run (a plan review's), which
/// a supervisor asks for under its registration (ADR-t3102-1 decision 2), is
/// given back only by that supervisor while it is there: another
/// supervisor's revocation is refused as `authorization_denied` and
/// recorded, whether it names the first one's registration as the lease or
/// no lease, and the token still holds. Once the first's registration
/// went, it is gone though its process runs on.
#[test]
fn another_supervisor_revokes_no_live_supervisors_telemetry_of_a_target_of_no_run() {
    use dagq::domain::LeaseToken;
    use dagq::domain::session_step::{StepTarget, TargetKind};
    let queue = queue();
    start(&queue);
    let pid = std::process::id();
    let (first, second) = (LeaseToken::new("it-first"), LeaseToken::new("it-second"));
    let mut db = SqliteQueue::open(&queue.db).unwrap();
    for lease in [&first, &second] {
        db.register_supervisor(lease, pid, 1, "test").unwrap();
    }
    drop(db);
    let (first_token, second_token) = (
        token(&queue, &Principal::supervisor(&first, pid)),
        token(&queue, &Principal::supervisor(&second, pid)),
    );
    let op = |token: &str, op: &str, args: Value| {
        call(
            &queue,
            Some(token),
            UseCase::Supervisor,
            json!({"op": op, "args": args}),
        )
    };
    let target = StepTarget::of(TargetKind::PlanReview, 1);
    let held = || {
        dagq::application::queue_service::TelemetryTokens::telemetry_credential(
            &dagq::infrastructure::queue_service::SystemServiceAccess,
            &queue.db,
            &target,
        )
        .is_some()
    };
    let issued = op(
        &first_token,
        "issue_telemetry",
        json!([target, null, first]),
    );
    assert_eq!(issued["ok"], true, "{issued}");
    assert!(held());
    for lease in [json!(first), Value::Null] {
        let before = events(&queue.db, "authorization_denied").len();
        let refused = op(&second_token, "revoke_telemetry", json!([target, lease]));
        assert_eq!(
            refused["error"]["code"], "authorization_denied",
            "{lease}: {refused}"
        );
        let denied = events(&queue.db, "authorization_denied");
        assert_eq!(denied.len() - before, 1, "{lease}");
        let last = &denied[denied.len() - 1]["payload"];
        assert_eq!(last["capability"], "token.issue", "{last}");
        assert!(
            last["why"].as_str().unwrap().contains("another parent's"),
            "{last}"
        );
        assert!(held(), "{lease}");
    }
    let given_back = op(&first_token, "revoke_telemetry", json!([target, first]));
    assert_eq!(given_back["result"], true, "{given_back}");
    assert!(!held());

    // Once the first's registration went (it stopped), the first is gone
    // though its process runs on: the second gives back the telemetry the
    // first asked for, and is issued another's over the first's.
    let other = StepTarget::of(TargetKind::PlanReview, 2);
    for target in [&target, &other] {
        let issued = op(
            &first_token,
            "issue_telemetry",
            json!([target, null, first]),
        );
        assert_eq!(issued["ok"], true, "{issued}");
    }
    SqliteQueue::open(&queue.db)
        .unwrap()
        .deregister_supervisor(&first)
        .unwrap();
    let before = events(&queue.db, "authorization_denied").len();
    let revoked = op(&second_token, "revoke_telemetry", json!([target, null]));
    assert_eq!(revoked["result"], true, "{revoked}");
    assert!(!held());
    let taken = op(
        &second_token,
        "issue_telemetry",
        json!([other, null, second]),
    );
    assert_eq!(taken["ok"], true, "{taken}");
    assert_eq!(events(&queue.db, "authorization_denied").len(), before);
}

/// The supervisor's records of the container's parts (the image's builds
/// and failures, the relays' attention, the runs that own users of the
/// dagq machine) are operations of its use case on the service's side:
/// its principal is answered, and any other (a worker, a review job, the
/// user) is refused with `authorization_denied`, and nothing is written.
#[test]
fn the_container_parts_records_are_answered_to_the_supervisor_alone() {
    use dagq::domain::LeaseToken;
    let queue = queue();
    start(&queue);
    let lease = LeaseToken::new("it-containers");
    SqliteQueue::open(&queue.db)
        .unwrap()
        .register_supervisor(&lease, std::process::id(), 1, "test")
        .unwrap();
    let supervisor = token(&queue, &Principal::supervisor(&lease, std::process::id()));
    let ops = [
        (
            "record_queue_event",
            json!(["container_image_built", {"reference": "localhost/dagq-q:ab"}]),
        ),
        (
            "latest_queue_event",
            json!([["container_image_built", "container_image_failed"]]),
        ),
        (
            "latest_queue_event",
            json!([["queue_relay_down", "queue_relay_running"]]),
        ),
        ("latest_runs_in_progress", json!([])),
        ("run", json!([queue.run.id()])),
    ];
    let op = |token: Option<&str>, op: &str, args: &Value| {
        call(
            &queue,
            token,
            UseCase::Supervisor,
            json!({"op": op, "args": args}),
        )
    };
    let worker = token(
        &queue,
        &Principal::worker(queue.run.id(), queue.run.task_id()),
    );
    let review = token(
        &queue,
        &Principal::of(&ActorContext::review_job(queue.run.id(), 1)),
    );
    for other in [Some(worker.as_str()), Some(&review), None] {
        for (name, args) in &ops {
            let refused = op(other, name, args);
            assert_eq!(
                refused["error"]["code"], "authorization_denied",
                "{name}: {refused}"
            );
        }
    }
    assert!(events(&queue.db, "container_image_built").is_empty());
    let denials = events(&queue.db, "authorization_denied");
    // The worker's and the review job's refusals, each as its actor.
    assert_eq!(denials.len(), 2 * ops.len(), "{denials:?}");
    assert!(
        denials
            .iter()
            .all(|denial| denial["payload"]["capability"] == "scheduler.supervise"),
        "{denials:?}"
    );

    for (name, args) in &ops {
        let answered = op(Some(&supervisor), name, args);
        assert_eq!(answered["ok"], true, "{name}: {answered}");
    }
    let built = events(&queue.db, "container_image_built");
    assert_eq!(built.len(), 1, "{built:?}");
    assert_eq!(built[0]["actor"]["role"], "supervisor", "{built:?}");
}

/// The answer of the queue's service at `queue_dir` to a `show` with
/// `token`, from this test's process.
fn show_with(queue_dir: &Path, token: &str) -> Value {
    let response = service::call(
        &service::socket_path(queue_dir),
        &ServiceRequest {
            api_version: API_VERSION,
            token: Some(token.to_owned()),
            env_role: None,
            queue: None,
            binding: None,
            use_case: UseCase::Show,
            params: json!({"id": 1}),
        },
        TIMEOUT,
    )
    .unwrap();
    serde_json::to_value(response).unwrap()
}

/// The token of a supervisor's registration goes with the registration's
/// end (ADR-t3102-1 decision 2): one a live supervisor's sweep prunes as
/// left behind (its process gone, its heartbeat expired) at the prune, and
/// the supervisor's own as it stops. Each is refused by the service after
/// (`unauthenticated`, `unknown_token`) and its file is gone, while the
/// live supervisor's token holds until its stop. The service revokes the
/// stopping one's at its `sign_out` request: a supervisor whose request
/// never reaches the service keeps its token, which its process does not
/// remove itself.
#[test]
fn a_supervisors_token_goes_with_the_end_of_its_registration() {
    use dagq::application::supervisor_service::ServiceCaller;
    use dagq::domain::{HEARTBEAT_TIMEOUT_SECS, LeaseToken};

    let (_fixture, repo, db) = idle_queue();
    let dagq = common::service::owned_executable(db.parent().unwrap());
    let queue_dir = db.parent().unwrap().to_path_buf();
    let credential = |token: &str, pid: u32| {
        service::credential_path(
            &queue_dir,
            &Principal::supervisor(&LeaseToken::new(token), pid).credential_key(),
        )
    };
    let refused = |token: &str| {
        let answer = show_with(&queue_dir, token);
        assert_eq!(answer["error"]["code"], "unauthenticated", "{answer}");
    };
    let supervisor = Running::with_options(
        &db,
        &repo,
        dagq::compose::SuperviseOptions {
            queue_service: Some(dagq::compose::QueueServiceOptions {
                executable: dagq.clone(),
                interval: Duration::ZERO,
                start_timeout: TIMEOUT,
                control: None,
            }),
            sweep_interval: Duration::ZERO,
            ..crate::runtime_support::supervise_options(1, false)
        },
    );
    let own = supervisor_token(&db);
    let own_file = credential(&own, std::process::id());
    let own_token = service::read_token(&own_file).unwrap();

    // A supervisor that signed in as the user's and registered, and whose
    // process is gone without pruning its registration.
    let (left, dead) = ("left-behind", common::lifecycle::dead_pid());
    service::SocketCaller::new(&db)
        .sign_in(&LeaseToken::new(left), dead)
        .unwrap();
    let left_file = credential(left, dead);
    let left_token = service::read_token(&left_file).unwrap();
    SqliteQueue::open(&db)
        .unwrap()
        .register_supervisor(&LeaseToken::new(left), dead, 1, dagq::VERSION)
        .unwrap();
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE supervisors SET heartbeat_at=?1 WHERE token=?2",
            rusqlite::params![
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs() as i64
                    - HEARTBEAT_TIMEOUT_SECS
                    - 60,
                left
            ],
        )
        .unwrap();
    {
        let _waiting = common::within(TIMEOUT * 3, "the left registration to be pruned");
        while left_file.exists()
            || SqliteQueue::open(&db)
                .unwrap()
                .supervisors()
                .unwrap()
                .iter()
                .any(|registration| registration.token == left)
        {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    refused(&left_token);
    // The live one's holds through its sweeps.
    supervisor.passes(2);
    assert!(own_file.exists());
    let answer = show_with(&queue_dir, &own_token);
    assert_ne!(answer["error"]["code"], "unauthenticated", "{answer}");

    supervisor.drain();
    assert!(!own_file.exists(), "{}", own_file.display());
    refused(&own_token);
    let reasons: Vec<Value> = events(&db, "queue_service_unauthenticated")
        .iter()
        .map(|event| event["payload"]["reason"].clone())
        .collect();
    assert_eq!(reasons, [json!("unknown_token"), json!("unknown_token")]);

    let (unsent, sign_outs) = crate::runtime_support::service::KeepsDown::wrap("sign_out");
    let supervisor = Running::with_options(
        &db,
        &repo,
        dagq::compose::SuperviseOptions {
            service_route: dagq::compose::ServiceRoute::InProcessThrough(unsent),
            ..crate::runtime_support::supervise_options(1, false)
        },
    );
    let kept = supervisor_token(&db);
    supervisor.passes(1);
    let kept_file = credential(&kept, std::process::id());
    let kept_token = service::read_token(&kept_file).unwrap();
    supervisor.drain();
    assert_eq!(sign_outs.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(kept_file.exists(), "{}", kept_file.display());
    let answer = show_with(&queue_dir, &kept_token);
    assert_ne!(answer["error"]["code"], "unauthenticated", "{answer}");
}

/// `up` and `down` revoke, through the service's `revoke_token` use case,
/// the token of each registration they prune (ADR-t3102-1 decision 2): a
/// supervisor that signed in, registered and died without giving it back.
/// The service refuses the token after (`unauthenticated`,
/// `unknown_token`) and its file is gone.
#[test]
fn up_and_down_revoke_the_tokens_of_the_registrations_they_prune() {
    use dagq::application::supervisor_service::ServiceCaller;
    use dagq::domain::LeaseToken;

    let mut fixture = common::lifecycle::fixture();
    fixture.options.queue_service = true;
    fixture.environment.current_exe =
        common::service::owned_executable(fixture.location.db.parent().unwrap());
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let processes = FakeProcesses::default();
    let db = fixture.location.db.clone();
    let queue_dir = db.parent().unwrap().to_path_buf();
    struct Down(PathBuf);
    impl Drop for Down {
        fn drop(&mut self) {
            let _ = invoke(&self.0, &["service", "stop"]);
        }
    }
    let _down = Down(db.clone());
    let first = common::lifecycle::up(&fixture, &launchd, &processes);
    assert_eq!(first["queue_service"]["outcome"], "started", "{first}");
    // A supervisor that signed in as the user's, registered and died.
    let died = |token: &str| {
        let pid = common::lifecycle::dead_pid();
        processes.dead.lock().unwrap().insert(pid);
        service::SocketCaller::new(&db)
            .sign_in(&LeaseToken::new(token), pid)
            .unwrap();
        SqliteQueue::open(&db)
            .unwrap()
            .register_supervisor(&LeaseToken::new(token), pid, 1, dagq::VERSION)
            .unwrap();
        let file = service::credential_path(
            &queue_dir,
            &Principal::supervisor(&LeaseToken::new(token), pid).credential_key(),
        );
        let value = service::read_token(&file).unwrap();
        (file, value)
    };
    let refused = |token: &str| {
        let answer = show_with(&queue_dir, token);
        assert_eq!(answer["error"]["code"], "unauthenticated", "{answer}");
    };

    let (up_file, up_token) = died("pruned-by-up");
    let again = common::lifecycle::up(&fixture, &launchd, &processes);
    assert_eq!(
        again["pruned_supervisors"][0]["token"], "pruned-by-up",
        "{again}"
    );
    assert!(!up_file.exists(), "{}", up_file.display());
    refused(&up_token);

    // `down --force` prunes before it stops the service, which is started
    // again to be asked.
    let (down_file, down_token) = died("pruned-by-down");
    let forced = common::lifecycle::down(&fixture, &launchd, &processes, false, true);
    assert_eq!(forced["queue_service"]["outcome"], "stopped", "{forced}");
    assert!(!down_file.exists(), "{}", down_file.display());
    common::service::serve(&db);
    refused(&down_token);
    let reasons: Vec<Value> = events(&db, "queue_service_unauthenticated")
        .iter()
        .map(|event| event["payload"]["reason"].clone())
        .collect();
    assert_eq!(reasons, [json!("unknown_token"), json!("unknown_token")]);
}

/// The service revokes as it starts a supervisor's token left over from a
/// registration that ended, as one is when the supervisor stopped the
/// service itself at the end of a `down`'s drain and so could not give it
/// back (ADR-t3102-1 decision 2): its file is gone and the service refuses
/// it. One whose registration is kept, and one of a supervisor not
/// registered yet whose process runs, stay.
#[test]
fn the_service_revokes_at_its_start_the_tokens_of_ended_registrations() {
    use dagq::application::ProcessControl;
    use dagq::domain::LeaseToken;
    use dagq::domain::queue_service::{SessionProcess, TokenIssuer};

    let queue = queue();
    let queue_dir = queue.dir().to_path_buf();
    let dead = common::lifecycle::dead_pid();
    let own = std::process::id();
    let process = |pid: u32| SessionProcess {
        pid,
        start: dagq::infrastructure::adapters::SystemProcesses
            .session_start(pid)
            .unwrap_or_else(|| "Thu Oct 23 10:00:00 2026".to_owned()),
    };
    let signed_in = |token: &str, pid: u32, asked: SessionProcess| {
        let issuer = TokenIssuer {
            parent: "user".to_owned(),
            process: Some(asked),
            registration: None,
        };
        let principal = Principal::supervisor(&LeaseToken::new(token), pid);
        let file = service::issue_by(&queue_dir, &principal, &issuer, 1)
            .unwrap()
            .file;
        let value = service::read_token(&file).unwrap();
        (file, value)
    };
    let (ended_file, ended) = signed_in("ended", dead, process(dead));
    let (kept_file, kept) = signed_in("kept", dead, process(dead));
    SqliteQueue::open(&queue.db)
        .unwrap()
        .register_supervisor(&LeaseToken::new("kept"), dead, 1, dagq::VERSION)
        .unwrap();
    let (starting_file, starting) = signed_in("starting", own, process(own));

    start(&queue);
    assert!(!ended_file.exists(), "{}", ended_file.display());
    let answer = show_with(&queue_dir, &ended);
    assert_eq!(answer["error"]["code"], "unauthenticated", "{answer}");
    for (file, token) in [(&kept_file, &kept), (&starting_file, &starting)] {
        assert!(file.exists(), "{}", file.display());
        let answer = show_with(&queue_dir, token);
        assert_ne!(answer["error"]["code"], "unauthenticated", "{answer}");
    }
}

/// A supervisor's token an older binary left, kept with no process that
/// asked for it (ADR-t3102-1 decision 2), goes by the supervisor's own
/// process, the pid its principal names: once its registration is gone and
/// that pid is gone, or another process started on it after the issue, the
/// service's start revokes it, and a live supervisor's sweep revokes one
/// left while the service runs. One whose supervisor still starts (its pid
/// runs since before the issue, not registered yet) and one whose
/// registration is kept stay.
#[test]
fn a_supervisors_token_an_older_binary_left_goes_once_its_registration_and_process_ended() {
    use dagq::domain::LeaseToken;

    let queue = queue();
    let queue_dir = queue.dir().to_path_buf();
    let dead = common::lifecycle::dead_pid();
    let own = std::process::id();
    let now = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap();
    // Issued as an older binary issued it: no process that asked is kept.
    let older = |token: &str, pid: u32, issued_at: i64| {
        let principal = Principal::supervisor(&LeaseToken::new(token), pid);
        let file = service::issue(&queue_dir, &principal, issued_at)
            .unwrap()
            .file;
        let value = service::read_token(&file).unwrap();
        (principal, file, value)
    };
    let refused = |file: &Path, value: &str| {
        assert!(!file.exists(), "{}", file.display());
        let answer = show_with(&queue_dir, value);
        assert_eq!(answer["error"]["code"], "unauthenticated", "{answer}");
    };
    let held = |file: &Path, value: &str| {
        assert!(file.exists(), "{}", file.display());
        let answer = show_with(&queue_dir, value);
        assert_ne!(answer["error"]["code"], "unauthenticated", "{answer}");
    };
    let (_, gone_file, gone) = older("gone", dead, now);
    // Its pid is this test's now, which started after the issue.
    let (_, reused_file, reused) = older("reused", own, 1);
    let (starting_principal, starting_file, starting) = older("starting", own, now);
    let (_, kept_file, kept) = older("kept", dead, now);
    let sweeper = LeaseToken::new("sweeper");
    let mut db = SqliteQueue::open(&queue.db).unwrap();
    db.register_supervisor(&LeaseToken::new("kept"), dead, 1, dagq::VERSION)
        .unwrap();
    db.register_supervisor(&sweeper, own, 1, dagq::VERSION)
        .unwrap();
    drop(db);

    start(&queue);
    refused(&gone_file, &gone);
    refused(&reused_file, &reused);
    held(&kept_file, &kept);
    held(&starting_file, &starting);

    let supervisor = token(&queue, &Principal::supervisor(&sweeper, own));
    let revoke = |principal: &Principal| {
        call(
            &queue,
            Some(&supervisor),
            UseCase::Supervisor,
            json!({"op": "revoke_principal", "args": [principal]}),
        )
    };
    let (swept_principal, swept_file, swept) = older("swept", dead, now);
    assert_eq!(revoke(&swept_principal)["result"], true);
    refused(&swept_file, &swept);
    assert_eq!(revoke(&starting_principal)["result"], false);
    held(&starting_file, &starting);
}

/// The supervisor a `down` asks to stop the service at the end of its drain
/// has the service revoke its token as it stops (ADR-t3102-1 decision 2):
/// once the drain is over, before any service starts again, its file is
/// gone and no record authenticates it, and a service started again
/// refuses it. The supervisor is on the service's socket, as `up` starts
/// one.
#[test]
fn the_supervisor_that_stops_the_service_after_down_leaves_no_token() {
    use dagq::domain::LeaseToken;
    use dagq::domain::queue_service::ServiceState::Stopped;

    let (_fixture, repo, db) = idle_queue();
    let dagq = common::service::owned_executable(db.parent().unwrap());
    let queue_dir = db.parent().unwrap().to_path_buf();
    struct Down(PathBuf);
    impl Drop for Down {
        fn drop(&mut self) {
            let _ = invoke(&self.0, &["service", "stop"]);
        }
    }
    let _down = Down(db.clone());
    let supervisor = Running::with_options(
        &db,
        &repo,
        dagq::compose::SuperviseOptions {
            queue_service: Some(dagq::compose::QueueServiceOptions {
                executable: dagq.clone(),
                interval: Duration::ZERO,
                start_timeout: TIMEOUT,
                control: None,
            }),
            service_route: dagq::compose::ServiceRoute::Socket,
            ..crate::runtime_support::supervise_options(1, false)
        },
    );
    let token = supervisor_token(&db);
    supervisor.passes(2);
    let file = service::credential_path(
        &queue_dir,
        &Principal::supervisor(&LeaseToken::new(&token), std::process::id()).credential_key(),
    );
    let value = service::read_token(&file).unwrap();
    request_stop(&db, &[&token]);
    supervisor.drain();
    assert_eq!(service::probe(&queue_dir).state, Stopped);
    assert!(!file.exists(), "{}", file.display());
    assert_eq!(service::lookup(&queue_dir, &value).unwrap(), None);

    common::service::serve(&db);
    let answer = show_with(&queue_dir, &value);
    assert_eq!(answer["error"]["code"], "unauthenticated", "{answer}");
}

/// A `down --wait` that stops the service as soon as the supervisor's
/// registration is gone, before the supervisor's own stop of it and its
/// sign-out, leaves no token either (ADR-t3102-1 decision 2): the
/// supervisor asked the service to revoke it as it stops before it removed
/// the registration. Once `down` returns, while the supervisor is still held
/// right after its deregistration, the file is gone and no record
/// authenticates it; a service started again refuses it.
#[test]
fn a_down_wait_that_stops_the_service_once_the_registration_is_gone_leaves_no_token() {
    use crate::runtime_support::headless::{pause_once, wait_for_file};
    use dagq::domain::LeaseToken;
    use dagq::domain::queue_service::ServiceState::Stopped;

    let (_fixture, repo, db) = idle_queue();
    let dagq = common::service::owned_executable(db.parent().unwrap());
    let queue_dir = db.parent().unwrap().to_path_buf();
    struct Down(PathBuf);
    impl Drop for Down {
        fn drop(&mut self) {
            let _ = invoke(&self.0, &["service", "stop"]);
        }
    }
    let _down = Down(db.clone());
    let (deregistered, go) = (queue_dir.join("deregistered"), queue_dir.join("go"));
    let options = dagq::compose::SuperviseOptions {
        queue_service: Some(dagq::compose::QueueServiceOptions {
            executable: dagq.clone(),
            interval: Duration::ZERO,
            start_timeout: TIMEOUT,
            control: None,
        }),
        service_route: dagq::compose::ServiceRoute::Socket,
        pauses: Some(pause_once(
            dagq::runtime::PausePoint::Deregistered,
            deregistered.clone(),
            go.clone(),
        )),
        ..crate::runtime_support::supervise_options(1, false)
    };
    let stop = options.stop.clone();
    let supervisor = Running::with_options(&db, &repo, options);
    // A failure lets the held supervisor go on before it is waited for.
    struct Go(PathBuf);
    impl Drop for Go {
        fn drop(&mut self) {
            let _ = fs::write(&self.0, "");
        }
    }
    let release = Go(go);
    let token = supervisor_token(&db);
    supervisor.passes(2);
    let file = service::credential_path(
        &queue_dir,
        &Principal::supervisor(&LeaseToken::new(&token), std::process::id()).credential_key(),
    );
    let value = service::read_token(&file).unwrap();

    let location =
        dagq::infrastructure::location::QueueLocation::explicit_in(&db, &queue_dir.join("home"));
    let launchd = FakeLaunchd::new(&db);
    let processes = FakeProcesses::default();
    let report = std::thread::scope(|scope| {
        let down = scope.spawn(|| {
            dagq::lifecycle::down(
                &location,
                &launchd,
                &processes,
                &dagq::lifecycle::DownOptions {
                    wait: true,
                    force: false,
                    poll: Duration::from_millis(20),
                },
            )
            .unwrap()
        });
        // The supervisor drains once `down` asked it to stop the service
        // and signalled it, and is held right after its deregistration.
        {
            let _waiting = common::within(TIMEOUT * 3, "down to ask for the service's stop");
            while events(&db, "queue_service_stop_requested").is_empty() {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        wait_for_file(&deregistered);
        let _waiting = common::within(TIMEOUT * 3, "down --wait to return");
        down.join().unwrap()
    });
    assert_eq!(report["queue_service"]["outcome"], "stopped", "{report}");
    assert_eq!(service::probe(&queue_dir).state, Stopped);
    assert!(!file.exists(), "{}", file.display());
    assert_eq!(service::lookup(&queue_dir, &value).unwrap(), None);

    drop(release);
    supervisor.drain();
    common::service::serve(&db);
    let answer = show_with(&queue_dir, &value);
    assert_eq!(answer["error"]["code"], "unauthenticated", "{answer}");
}
