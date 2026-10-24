//! The queue service a test's workers and jobs reach (goal 82's stage
//! (3)): their `dagq` runs in client mode with the service's socket and a
//! token, never the queue's path, so a test whose stub agent runs `dagq`
//! starts the queue's service first ([`serve`]), or the stub starts it on
//! its first `dagq` ([`started_by_a_stub`]), and the test stops it when
//! done ([`Served`], or [`unserve`]). A service also stops itself within seconds
//! of its queue's directory going away, and one a test started within
//! seconds of the test's process going ([`OwnedByTest`]): a timeout's
//! `process::exit` or a SIGKILL skips the drops and leaves the directory
//! (task 1352).

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    process::Command,
    sync::{LazyLock, Mutex, MutexGuard, PoisonError},
};

use crate::common::{self, Bounded, WithoutActor};
use common::cli::{invoke, ok};
use dagq::application::TaskStore;
use dagq::domain::queue_service::{API_VERSION, Principal, ServiceRequest, UseCase};
use dagq::domain::{ClaimOutcome, TaskAction, TaskRun};
use dagq::infrastructure::queue_service as service;
use serde_json::Value;
use std::time::Duration;

/// How long a call of a test to its queue's service waits.
const TIMEOUT: Duration = Duration::from_secs(10);

/// The queues whose service a test started and has not stopped.
static SERVED: LazyLock<Mutex<HashSet<PathBuf>>> = LazyLock::new(Default::default);

fn served() -> MutexGuard<'static, HashSet<PathBuf>> {
    SERVED.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Has the queue service the command starts, itself or through a
/// supervisor it runs, stop once this test process is gone
/// ([`dagq::domain::queue_service::OWNER_PID_ENV`]).
pub trait OwnedByTest {
    fn owned_by_test(&mut self) -> &mut Self;
}

impl OwnedByTest for Command {
    fn owned_by_test(&mut self) -> &mut Self {
        self.env(
            dagq::domain::queue_service::OWNER_PID_ENV,
            std::process::id().to_string(),
        )
    }
}

/// A `dagq` in `dir` that runs the tests' binary [`OwnedByTest`], for a
/// supervisor the test runs in its own process to start the service with:
/// the service would inherit no owner from the test's environment.
pub fn owned_executable(dir: &Path) -> PathBuf {
    let wrapper = dir.join("owned-dagq");
    let owner = std::process::id().to_string();
    crate::common::template::script_env(
        &wrapper,
        "#!/bin/sh\nexport DAGQ_SERVICE_OWNER_PID\nexec \"$OWNED_DAGQ\" \"$@\"\n",
        &[
            (dagq::domain::queue_service::OWNER_PID_ENV, &owner),
            ("OWNED_DAGQ", env!("CARGO_BIN_EXE_dagq")),
        ],
    );
    wrapper
}

/// A process that stands for a test's, given to a supervisor as its owner
/// ([`dagq::domain::queue_service::OWNER_PID_ENV`]) so that the test can end
/// it. It ends with this test's process too, so what it owns does not
/// outlive the test.
pub struct StandInOwner(crate::common::KillOnDrop);

impl StandInOwner {
    pub fn start() -> Self {
        Self(crate::common::KillOnDrop::new(
            Command::new("sh")
                .args([
                    "-c",
                    "while kill -0 \"$PPID\" 2>/dev/null; do sleep 1; done",
                ])
                // Its `sleep`, left for a second after a SIGKILL, holds none
                // of the test's output.
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap(),
            "the stand-in owner",
        ))
    }

    pub fn pid(&mut self) -> u32 {
        self.0.child().id()
    }

    /// End it with SIGKILL, as a test's process ends at a timeout, and reap
    /// it: until then it is a zombie, which its supervisor still finds.
    pub fn kill(&mut self) {
        let pid = libc::pid_t::try_from(self.pid()).unwrap();
        // SAFETY: kill(2) takes no pointer.
        unsafe { libc::kill(pid, libc::SIGKILL) };
        let _waiting =
            crate::common::within(crate::common::STEP_LIMIT, "the killed owner to be reaped");
        self.0.child().wait().unwrap();
    }
}

fn dagq(db: &Path, args: &[&str]) -> std::process::Output {
    // A `service` command takes over a service started for the test.
    host_client_env(db, &[], args);
    Command::new(env!("CARGO_BIN_EXE_dagq"))
        .without_actor_env()
        .owned_by_test()
        .arg("--db")
        .arg(db)
        .args(args)
        .bounded_output()
        .unwrap()
}

/// The variables a read of the queue service depends on: the service
/// reads with its own, so a test's service runs with the caller's.
const READ_ENV: [&str; 7] = [
    "PATH",
    "HOME",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "TZ",
    "CLAUDE_CONFIG_DIR",
    "CODEX_HOME",
];

/// The file beside a queue that says the helper started its service for
/// the test ([`serve_for_test`]), with the [`READ_ENV`] it runs with.
fn test_service_file(dir: &Path) -> PathBuf {
    dir.join("test-service.env")
}

/// What a test's `dagq` of the user, the inbox or a planner with `env`
/// needs for `args` on the queue at `db`, since it never opens the DB for
/// a command the queue service answers (ADR-t3105-1 decision 3): the
/// queue's service, started for the test when none ever ran for the queue
/// or started again when the one started for it reads with other
/// [`READ_ENV`] ([`serve_for_test`]), and for the inbox and a planner the
/// environment names a token of its own unless `env` names one: the
/// inbox's bound to this test's process as its session, a planner's to
/// the planner its actor id names ([`role_planner`] when it names none).
/// The variables to add to the command's environment; none for another
/// role, a command the service does not answer, a queue that is not there,
/// or a queue whose service the test runs itself (a `service stop` hands
/// the one started for the test over to it).
pub fn host_client_env(db: &Path, env: &[(&str, &str)], args: &[&str]) -> Vec<(String, String)> {
    use dagq::domain::queue_service::{CREDENTIAL_FILE_ENV, LOCK_FILE, SERVICE_DIR};
    let named = |name: &str| {
        env.iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| *value)
    };
    let role = named(dagq::domain::actor::ROLE_ENV);
    let Ok(db) = std::fs::canonicalize(db) else {
        return Vec::new();
    };
    let dir = db.parent().unwrap();
    if args.first() == Some(&"service") {
        let _ = std::fs::remove_file(test_service_file(dir));
        return Vec::new();
    }
    let routed = args.first() == Some(&"watch") || UseCase::of_command(args).is_some();
    if !matches!(role, None | Some("inbox" | "planner")) || !routed {
        return Vec::new();
    }
    let wanted: Vec<(String, String)> = READ_ENV
        .iter()
        .filter_map(|name| named(name).map(|value| ((*name).to_owned(), value.to_owned())))
        .collect();
    let started = std::fs::read(test_service_file(dir))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Vec<(String, String)>>(&bytes).ok());
    match started {
        Some(running) => {
            // What the service reads with: what it was started with, else
            // the test's own. A caller that sets another value of one (but
            // `PATH`, which only locates programs) gets a service of its
            // own, which keeps the rest.
            let value = |name: &str| {
                running
                    .iter()
                    .find(|(set, _)| set == name)
                    .map(|(_, value)| value.clone())
                    .or_else(|| std::env::var(name).ok())
            };
            let differs = wanted
                .iter()
                .any(|(name, set)| name != "PATH" && value(name).as_deref() != Some(set));
            let up =
                service::probe(dir).state == dagq::domain::queue_service::ServiceState::Running;
            if differs || !up {
                let mut env = running;
                env.retain(|(name, _)| !wanted.iter().any(|(set, _)| set == name));
                env.extend(wanted);
                stop_for_test(dir);
                serve_for_test(&db, &env);
            }
        }
        None if !dir.join(SERVICE_DIR).join(LOCK_FILE).exists() => {
            serve_for_test(&db, &wanted);
        }
        None => {}
    }
    if named(CREDENTIAL_FILE_ENV).is_some() {
        return Vec::new();
    }
    let mut handed = Vec::new();
    let principal = match role {
        Some("inbox") => {
            let pid = std::process::id();
            Principal::inbox(dagq::domain::queue_service::SessionProcess {
                pid,
                start: dagq::infrastructure::adapters::session_start(pid).unwrap(),
            })
        }
        Some("planner") => match named(dagq::domain::actor::ACTOR_ID_ENV) {
            Some(actor) => {
                let Some(id) = actor
                    .strip_prefix("planner:")
                    .and_then(|id| id.parse().ok())
                else {
                    return Vec::new();
                };
                Principal::planner(id)
            }
            // A planner the test names by its role only is the one the
            // test opened for it: every planner on the host is a
            // runtime's, with an id.
            None => {
                let id = role_planner(&db);
                handed.push((
                    dagq::domain::actor::ACTOR_ID_ENV.to_owned(),
                    format!("planner:{id}"),
                ));
                Principal::planner(id)
            }
        },
        _ => return Vec::new(),
    };
    let held = service::credential_path_of(dir, &principal);
    let file = if held.is_file() {
        held
    } else {
        service::issue(dir, &principal, 1).unwrap().file
    };
    handed.push((
        CREDENTIAL_FILE_ENV.to_owned(),
        file.to_str().unwrap().to_owned(),
    ));
    handed
}

/// Start a service of the queue at `db` for this test with `env` (of
/// [`READ_ENV`]), as `service serve`, which records no event, and wait
/// until it answers. It stops when the test's process or the queue's
/// directory goes ([`OwnedByTest`]).
pub fn serve_for_test(db: &Path, env: &[(String, String)]) {
    let dir = db.parent().unwrap();
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("test-service.log"))
        .unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_dagq"));
    command.without_actor_env().owned_by_test();
    command.envs(env.iter().map(|(key, value)| (key, value)));
    // A child left running: it is reaped with the test's process.
    drop(
        command
            .arg("--db")
            .arg(db)
            .args(["service", "serve"])
            .stdin(std::process::Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap(),
    );
    let _waiting = common::within(TIMEOUT, "the test's queue service to answer");
    while service::probe(dir).state != dagq::domain::queue_service::ServiceState::Running {
        std::thread::sleep(Duration::from_millis(20));
    }
    std::fs::write(test_service_file(dir), serde_json::to_vec(env).unwrap()).unwrap();
}

/// Stop the service [`serve_for_test`] started for the queue in `dir`,
/// and wait until it no longer holds the queue.
fn stop_for_test(dir: &Path) {
    use dagq::domain::queue_service::{SERVICE_DIR, STATE_FILE};
    let _ = std::fs::remove_file(test_service_file(dir));
    let pid = std::fs::read(dir.join(SERVICE_DIR).join(STATE_FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|record| record["pid"].as_i64());
    if let Some(pid) = pid.and_then(|pid| libc::pid_t::try_from(pid).ok()) {
        // SAFETY: kill(2) takes no pointer.
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }
    let _waiting = common::within(TIMEOUT, "the test's queue service to stop");
    while service::probe(dir).state != dagq::domain::queue_service::ServiceState::Stopped {
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The planner of the queue at `db` a test's `dagq` named by the role
/// `planner` only runs as: a runtime's, opened the first time and named
/// in a file beside the queue after.
fn role_planner(db: &Path) -> i64 {
    let file = db.with_file_name("test-role-planner");
    if let Some(id) = std::fs::read_to_string(&file)
        .ok()
        .and_then(|id| id.trim().parse().ok())
    {
        return id;
    }
    let id = dagq::infrastructure::sqlite::SqliteQueue::open(db)
        .unwrap()
        .open_planner(dagq::domain::PlannerOrigin::Runtime, None)
        .unwrap()
        .id
        .as_i64();
    std::fs::write(&file, id.to_string()).unwrap();
    id
}

/// Start the service of the queue at `db` unless this test process did
/// already.
pub fn serve(db: &Path) {
    let mut served = served();
    if served.contains(db) {
        return;
    }
    let output = dagq(db, &["service", "start"]);
    assert!(
        output.status.success(),
        "the queue service of {} did not start: {}",
        db.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    served.insert(db.to_owned());
}

/// The file a test's stub writes beside the queue at `db` once it started
/// the queue's service itself (the runtime tests' headless stubs start it on
/// their first client-mode `dagq`).
pub fn started_by_a_stub(db: &Path) -> PathBuf {
    db.with_file_name("queue-service-started-by-a-stub")
}

/// Stop the service [`serve`] or a stub ([`started_by_a_stub`]) started for
/// the queue at `db`, if one did.
pub fn unserve(db: &Path) {
    let by_a_stub = std::fs::remove_file(started_by_a_stub(db)).is_ok();
    if served().remove(db) || by_a_stub {
        let _ = dagq(db, &["service", "stop"]);
    }
}

/// The queue's service while it is held: started now, stopped when it
/// goes (the test returned or panicked).
pub struct Served(PathBuf);

impl Served {
    pub fn start(db: &Path) -> Self {
        serve(db);
        Self(db.to_owned())
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        unserve(&self.0);
    }
}

/// A queue with one task claimed as run `run`.
pub struct Queue {
    _dir: tempfile::TempDir,
    pub db: PathBuf,
    pub run: TaskRun,
}

impl Queue {
    pub fn dir(&self) -> &Path {
        self.db.parent().unwrap()
    }
}

impl Drop for Queue {
    /// No service outlives its test.
    fn drop(&mut self) {
        let _ = invoke(&self.db, &["service", "stop"]);
    }
}

pub fn queue() -> Queue {
    let (dir, mut queue) = common::queue::fixture();
    let task = queue.add(common::queue::new_task("served")).unwrap();
    queue
        .transition(task.id(), TaskAction::BypassReview)
        .unwrap();
    let ClaimOutcome::Claimed { run } = queue.claim(&common::queue::base()).unwrap() else {
        panic!("nothing claimed");
    };
    Queue {
        db: dir.path().join("queue.db"),
        _dir: dir,
        run: *run,
    }
}

pub fn start(queue: &Queue) -> Value {
    ok(&queue.db, &["service", "start"])
}

pub fn token(queue: &Queue, principal: &Principal) -> String {
    let issued = service::issue(queue.dir(), principal, 1).unwrap();
    service::read_token(&issued.file).unwrap()
}

/// `use_case` with `params` to the queue's service with `token`, from this
/// test's process: with no token, the user's (ADR-t3102-1 decision 1).
pub fn call(queue: &Queue, token: Option<&str>, use_case: UseCase, params: Value) -> Value {
    request(queue, token, None, use_case, params)
}

/// [`call`] with no token from an environment that names `role`, which the
/// service never takes for the user's.
pub fn call_as_role(queue: &Queue, role: &str, use_case: UseCase, params: Value) -> Value {
    request(queue, None, Some(role), use_case, params)
}

fn request(
    queue: &Queue,
    token: Option<&str>,
    env_role: Option<&str>,
    use_case: UseCase,
    params: Value,
) -> Value {
    let response = service::call(
        &service::socket_path(queue.dir()),
        &ServiceRequest {
            api_version: API_VERSION,
            token: token.map(str::to_owned),
            env_role: env_role.map(str::to_owned),
            queue: None,
            binding: None,
            use_case,
            params,
        },
        TIMEOUT,
    )
    .unwrap();
    serde_json::to_value(response).unwrap()
}

/// The queue's events of `kind`, oldest first, as `events --full` prints
/// them, read by the test as a library: a served queue's `dagq` reads
/// through its service, which may be stopped by then.
pub fn events(db: &Path, kind: &str) -> Vec<Value> {
    dagq::infrastructure::sqlite::SqliteQueue::open(db)
        .unwrap()
        .all_events()
        .unwrap()
        .into_iter()
        .filter(|event| event.kind.as_str() == kind)
        .map(|event| serde_json::to_value(event).unwrap())
        .collect()
}

/// The queue's `telemetry_token_*` events of the targets of no run of
/// `kind` (ADR-t1662-3 decision 11), in order, each as its target's id, its
/// kind and its reason (`None` for an issue).
pub fn target_token_events(
    db: &Path,
    kind: dagq::domain::session_step::TargetKind,
) -> Vec<(String, &'static str, Option<String>)> {
    use dagq::domain::EventKind;
    dagq::infrastructure::sqlite::SqliteQueue::open(db)
        .unwrap()
        .all_events()
        .unwrap()
        .into_iter()
        .filter(|event| event.run_id.is_none() && event.payload["target"]["kind"] == kind.as_str())
        .filter_map(|event| {
            let recorded = match event.kind.known()? {
                EventKind::TelemetryTokenIssued => "issued",
                EventKind::TelemetryTokenRevoked => "revoked",
                _ => return None,
            };
            Some((
                event.payload["target"]["id"].as_str()?.to_owned(),
                recorded,
                event.payload["reason"].as_str().map(str::to_owned),
            ))
        })
        .collect()
}

/// The target the control side registered `session` to
/// ([`dagq::application::SessionOwners`]): `<kind>:<id>`, `None` for one
/// it never registered.
pub fn session_owner(db: &Path, session: &str) -> Option<String> {
    use dagq::application::SessionOwners;
    dagq::infrastructure::sqlite::SqliteQueue::open(db)
        .unwrap()
        .session_owner(session)
        .unwrap()
        .map(|owner| format!("{}:{}", owner.target.kind.as_str(), owner.target.id))
}

/// Serve the queue at `db` and issue the token of the session wrapper of
/// `target` (of `task`, a run's), as the supervisor does before it starts
/// the wrapper (ADR-t3102-1 decisions 2 and 4): a test that starts a
/// wrapper's process itself, which reaches the queue only through its
/// service.
pub fn wrapper_token(
    db: &Path,
    target: &dagq::domain::session_step::StepTarget,
    task: Option<dagq::domain::TaskId>,
) {
    serve(db);
    service::issue(db.parent().unwrap(), &Principal::wrapper(target, task), 1).unwrap();
}
