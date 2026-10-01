//! The queue service on the host (docs/design/queue-service.md,
//! ADR-t1233-1, ADR-t1233-4, ADR-t1233-5): the `dagq` binary's service
//! answers `ask`, `show` and `note` on its unix socket for the principal a
//! token names, refuses and records what the policy refuses on its side,
//! refuses a caller without a principal and one of another API version;
//! `service start|stop|status`, `status` and `doctor` report it; `up` and
//! `down` start and stop it, and a supervisor starts it again and holds
//! its claims while it cannot.

use crate::common;

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::Duration,
};

use common::cli::{invoke, ok};
use common::lifecycle::{FakeCmux, FakeLaunchd, FakeProcesses};
use dagq::{
    application::{RunLog, TaskStore},
    domain::{
        ActorContext, ClaimOutcome, TaskAction, TaskRun,
        queue_service::{API_VERSION, Principal, ServiceErrorCode, ServiceRequest, UseCase},
    },
    infrastructure::{
        queue_service::{self as service, SystemQueueService},
        sqlite::SqliteQueue,
    },
};
use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(10);

/// A queue with one task claimed as run `run`, and a stub cmux beside it.
pub(crate) struct Queue {
    _dir: tempfile::TempDir,
    pub(crate) db: PathBuf,
    pub(crate) cmux: PathBuf,
    pub(crate) run: TaskRun,
}

impl Queue {
    pub(crate) fn dir(&self) -> &Path {
        self.db.parent().unwrap()
    }
}

impl Drop for Queue {
    /// No service outlives its test.
    fn drop(&mut self) {
        let _ = invoke(&self.db, &["service", "stop"]);
    }
}

pub(crate) fn queue() -> Queue {
    let (dir, mut queue) = common::queue::fixture();
    let task = queue.add(common::queue::new_task("served")).unwrap();
    queue
        .transition(task.id(), TaskAction::BypassReview)
        .unwrap();
    let ClaimOutcome::Claimed { run } = queue.claim(&common::queue::base()).unwrap() else {
        panic!("nothing claimed");
    };
    let cmux = dir.path().join("cmux-stub");
    fs::write(
        &cmux,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" >> '{}'\n",
            dir.path().join("notifications").display()
        ),
    )
    .unwrap();
    fs::set_permissions(&cmux, fs::Permissions::from_mode(0o755)).unwrap();
    Queue {
        db: dir.path().join("queue.db"),
        _dir: dir,
        cmux,
        run: *run,
    }
}

pub(crate) fn start(queue: &Queue) -> Value {
    ok(
        &queue.db,
        &["service", "start", "--cmux", queue.cmux.to_str().unwrap()],
    )
}

pub(crate) fn token(queue: &Queue, principal: &Principal) -> String {
    let issued = service::issue(queue.dir(), principal, 1).unwrap();
    service::read_token(&issued.file).unwrap()
}

pub(crate) fn call(queue: &Queue, token: Option<&str>, use_case: UseCase, params: Value) -> Value {
    let response = service::call(
        &service::socket_path(queue.dir()),
        &ServiceRequest {
            api_version: API_VERSION,
            token: token.map(str::to_owned),
            use_case,
            params,
        },
        TIMEOUT,
    )
    .unwrap();
    serde_json::to_value(response).unwrap()
}

pub(crate) fn events(db: &Path, kind: &str) -> Vec<Value> {
    ok(db, &["events", "--all", "--full", "--kind", kind])["events"]
        .as_array()
        .unwrap()
        .clone()
}

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

    // The worker asks on its own run, recorded as itself, and the inbox
    // is told through the service's cmux.
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

    // No principal: no token, one never issued, one revoked.
    for token in [None, Some("f".repeat(64))] {
        let response = call(&queue, token.as_deref(), UseCase::Show, json!({"id": 1}));
        assert_eq!(response["error"]["code"], "unauthenticated", "{response}");
    }
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
    for args in [&["status"][..], &["doctor"], &["service", "status"]] {
        let report = ok(&queue.db, args);
        let view = if args == ["service", "status"] {
            &report
        } else {
            &report["queue_service"]
        };
        assert_eq!(view["state"], "stopped", "{args:?} {view}");
        assert_eq!(view["socket"], json!(service::socket_path(queue.dir())));
        assert_eq!(view["pid"], Value::Null);
        assert_eq!(view["client_api_version"], API_VERSION);
        assert_eq!(view["attention"], false);
    }
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
    }
}

/// A supervisor at work starts the service; one that cannot start it
/// claims nothing, and the inbox gets `queue_service_down` until the
/// service runs again.
#[test]
fn the_supervisor_starts_the_service_and_holds_its_claims_while_it_is_down() {
    use crate::runtime_support::{
        TestWorkspace, VALID_AGENT, fixture, supervise_options, supervise_with,
    };
    let (_fixture, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let options = |executable: &Path| dagq::compose::SuperviseOptions {
        queue_service: Some(dagq::compose::QueueServiceOptions {
            executable: executable.to_path_buf(),
            cmux: PathBuf::from("/nonexistent/cmux"),
            interval: Duration::ZERO,
            start_timeout: TIMEOUT,
            control: None,
        }),
        ..supervise_options(1, true)
    };
    let control = SystemQueueService::new(&db, Path::new("dagq"), Path::new("cmux"));
    struct Stop(SystemQueueService);
    impl Drop for Stop {
        fn drop(&mut self) {
            use dagq::application::queue_service::QueueServiceControl;
            let _ = self.0.stop(TIMEOUT);
        }
    }
    let _stop = Stop(control.clone());
    let runs = || {
        RunLog::all_runs(&SqliteQueue::open(&db).unwrap())
            .unwrap()
            .len()
    };

    // A binary that is not there: no service, no claim, the attention.
    let missing = repo.join("no-dagq");
    supervise_with(&db, &repo, &backend, &options(&missing)).unwrap();
    assert_eq!(runs(), 0);
    let down = events(&db, "queue_service_down");
    assert_eq!(down.len(), 1);
    assert_eq!(down[0]["payload"]["reason"], "start_failed");
    let attention = ok(&db, &["status"])["attention"].clone();
    let entry = attention
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["kind"] == "queue_service_down")
        .unwrap_or_else(|| panic!("{attention}"));
    assert_eq!(entry["next"], "dagq service status");
    // Told once, however many passes fail.
    supervise_with(&db, &repo, &backend, &options(&missing)).unwrap();
    assert_eq!(events(&db, "queue_service_down").len(), 1);
    assert_eq!(runs(), 0);

    // The binary: the service runs, the attention ends, the task is claimed.
    let dagq = PathBuf::from(env!("CARGO_BIN_EXE_dagq"));
    supervise_with(&db, &repo, &backend, &options(&dagq)).unwrap();
    let started = events(&db, "queue_service_started");
    assert_eq!(started.len(), 1);
    assert_eq!(started[0]["payload"]["by"], "supervisor");
    assert_eq!(started[0]["payload"]["build"], dagq::VERSION);
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
    assert_eq!(runs(), 1);
    // Stopped by a person, started again at the next look.
    {
        use dagq::application::queue_service::QueueServiceControl;
        assert!(control.stop(TIMEOUT).unwrap().is_some());
    }
    supervise_with(&db, &repo, &backend, &options(&dagq)).unwrap();
    assert_eq!(events(&db, "queue_service_started").len(), 2);
}

/// `up` starts the service before the supervisor and reuses it after;
/// `down` stops it once no supervisor runs.
#[test]
fn up_starts_the_service_and_down_stops_it() {
    let mut fixture = common::lifecycle::fixture();
    fixture.options.queue_service = true;
    fixture.environment.current_exe = PathBuf::from(env!("CARGO_BIN_EXE_dagq"));
    let cmux = FakeCmux::default();
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

    let first = common::lifecycle::up(&fixture, &cmux, &launchd, &processes);
    assert_eq!(first["queue_service"]["outcome"], "started", "{first}");
    assert_eq!(first["queue_service"]["service"]["state"], "running");
    let second = common::lifecycle::up(&fixture, &cmux, &launchd, &processes);
    assert_eq!(second["queue_service"]["outcome"], "reused", "{second}");
    let queue = SqliteQueue::open(&db).unwrap();
    let started = queue
        .latest_queue_event(&["queue_service_started"])
        .unwrap()
        .unwrap();
    assert_eq!(started.payload["by"], "up");

    // The supervisor `up` registered is this process: `down` drains it,
    // leaves the service to the drain, and a `--force` stops both.
    let draining = common::lifecycle::down(&fixture, &cmux, &launchd, &processes, false, false);
    assert_eq!(
        draining["queue_service"]["outcome"], "left_to_the_drain",
        "{draining}"
    );
    let asked = queue
        .latest_queue_event(&["queue_service_stop_requested"])
        .unwrap()
        .unwrap();
    assert_eq!(asked.payload["by"], "down");
    let forced = common::lifecycle::down(&fixture, &cmux, &launchd, &processes, false, true);
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
    let again = common::lifecycle::down(&fixture, &cmux, &launchd, &processes, false, false);
    assert!(again.get("queue_service").is_none(), "{again}");
}

/// A supervisor of a queue, in its own thread until `stop`: with the
/// fixture's ready task cancelled, it claims nothing and only keeps the
/// service, every pass (`interval` zero).
struct Running {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    passes: std::sync::Arc<std::sync::atomic::AtomicU64>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Running {
    fn start(
        db: &Path,
        repo: &Path,
        executable: &Path,
        control: Option<std::sync::Arc<dyn dagq::application::queue_service::QueueServiceControl>>,
    ) -> Self {
        use crate::runtime_support::{TestWorkspace, VALID_AGENT, supervise_options};
        let options = dagq::compose::SuperviseOptions {
            queue_service: Some(dagq::compose::QueueServiceOptions {
                executable: executable.to_path_buf(),
                cmux: PathBuf::from("/nonexistent/cmux"),
                interval: Duration::ZERO,
                start_timeout: TIMEOUT,
                control: control.map(dagq::compose::QueueServiceControlPort),
            }),
            ..supervise_options(1, false)
        };
        let (stop, passes) = (options.stop.clone(), options.passes.clone());
        let (db, repo) = (db.to_path_buf(), repo.to_path_buf());
        let thread = std::thread::spawn(move || {
            let backend = TestWorkspace::new(&db, false, VALID_AGENT);
            crate::runtime_support::supervise_with(&db, &repo, &backend, &options).unwrap();
        });
        Self {
            stop,
            passes,
            thread: Some(thread),
        }
    }

    /// Wait until `passes` more passes ran.
    fn passes(&self, passes: u64) {
        use std::sync::atomic::Ordering;
        let until = self.passes.load(Ordering::SeqCst) + passes;
        let _waiting = common::within(TIMEOUT * 3, format!("{passes} passes"));
        while self.passes.load(Ordering::SeqCst) < until {
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Ask it to drain, as `down` does, and wait for it to end.
    fn drain(mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _waiting = common::within(TIMEOUT * 3, "the supervisor to drain");
            thread.join().unwrap();
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The fixture's queue with its ready task cancelled: a supervisor of it
/// claims nothing.
fn idle_queue() -> (crate::runtime_support::Fixture, PathBuf, PathBuf) {
    let (fixture, repo, db) = crate::runtime_support::fixture();
    for task in ok(&db, &["list"])["tasks"].as_array().unwrap() {
        ok(&db, &["cancel", &task["id"].to_string()]);
    }
    (fixture, repo, db)
}

fn supervisor_token(db: &Path) -> String {
    let _waiting = common::within(TIMEOUT * 3, "the supervisor to register");
    loop {
        if let Some(registration) = SqliteQueue::open(db)
            .unwrap()
            .supervisors()
            .unwrap()
            .first()
        {
            return registration.token.to_string();
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn request_stop(db: &Path, tokens: &[&str]) {
    RunLog::record_queue_event(
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
    let dagq = PathBuf::from(env!("CARGO_BIN_EXE_dagq"));
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
/// then left to a person: `queue_service_down` with `restart_limit`, once.
#[test]
fn a_service_that_keeps_dying_is_left_to_a_person_after_three_starts() {
    let (_fixture, repo, db) = idle_queue();
    let fake = std::sync::Arc::new(FakeService::default());
    let supervisor = Running::start(&db, &repo, Path::new("/unused"), Some(fake.clone()));
    let _waiting = common::within(TIMEOUT * 3, "queue_service_down");
    while events(&db, "queue_service_down").is_empty() {
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
    let down = events(&db, "queue_service_down");
    assert_eq!(down.len(), 1, "{down:?}");
    assert_eq!(down[0]["payload"]["reason"], "restart_limit");
    let attention = ok(&db, &["status"])["attention"].clone();
    assert!(
        attention.as_array().unwrap().iter().any(
            |entry| entry["kind"] == "queue_service_down" && entry["status"] == "restart_limit"
        ),
        "{attention}"
    );
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
