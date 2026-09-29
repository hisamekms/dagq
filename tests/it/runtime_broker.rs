//! Runtime tests: the supervisor and `down` keep the queue's resource
//! broker (ADR-t827-3 decisions 2 and 3) through a fake podman: the mode
//! `disabled` calls none, a mode other than it makes the broker ready off
//! the loop (the image's build holds no claim), restarts the container
//! after three failed looks at its health, tells the inbox when it still
//! fails, and `down` stops it after the drain while a handoff does not.
use crate::common::lifecycle::{FakeCmux, FakeLaunchd, FakeProcesses};
use crate::runtime_support;

use dagq::{
    application::broker::{BrokerResult, HealthProbe, HostLock, ImageSource, Podman, PodmanOutput},
    compose::{BrokerOptions, OneShot},
    infrastructure::{broker_podman::BrokerState, broker_queue::BrokerPorts},
    lifecycle::DownOptions,
};
use dagq_broker_protocol::HealthResponse;
use runtime_support::*;
use std::sync::atomic::AtomicBool;

const RUNNING: &str = r#"[{"Name":"dagq","Running":true}]"#;
const BUSY: &str =
    r#"[{"Name":"dagq","Running":false},{"Name":"podman-machine-default","Running":true}]"#;

/// A podman that plays dagq's machine, image and container, and records
/// every command. `build` waits while `build_blocked`; a container `rm`
/// clears `broken` when `rm_fixes`.
#[derive(Default)]
struct FakePodman {
    calls: Mutex<Vec<String>>,
    machines: Mutex<String>,
    image: AtomicBool,
    container: AtomicBool,
    build_blocked: AtomicBool,
    broken: Arc<AtomicBool>,
    rm_fixes: AtomicBool,
}

impl FakePodman {
    fn new(machines: &str) -> Arc<Self> {
        let podman = Self::default();
        *podman.machines.lock().unwrap() = machines.to_owned();
        Arc::new(podman)
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    fn called(&self, part: &str) -> usize {
        self.calls()
            .iter()
            .filter(|call| call.contains(part))
            .count()
    }
}

fn answer(success: bool, stdout: &str) -> PodmanOutput {
    PodmanOutput {
        success,
        code: Some(if success { 0 } else { 1 }),
        stdout: stdout.to_owned(),
        stderr: String::new(),
    }
}

impl Podman for FakePodman {
    fn run(&self, args: &[String]) -> BrokerResult<PodmanOutput> {
        let line = args.join(" ");
        self.calls.lock().unwrap().push(line.clone());
        let words: Vec<&str> = args.iter().map(String::as_str).collect();
        let rest = match words.as_slice() {
            ["--connection", _, rest @ ..] => rest,
            all => all,
        };
        Ok(match rest {
            ["machine", "list", ..] => answer(true, &self.machines.lock().unwrap()),
            ["image", "exists", ..] => answer(self.image.load(Ordering::SeqCst), ""),
            ["build", ..] => {
                let deadline = Instant::now() + Duration::from_secs(60);
                while self.build_blocked.load(Ordering::SeqCst) && Instant::now() < deadline {
                    thread::sleep(Duration::from_millis(10));
                }
                self.image.store(true, Ordering::SeqCst);
                answer(true, "")
            }
            ["container", "exists", ..] => answer(self.container.load(Ordering::SeqCst), ""),
            ["container", "inspect", ..] => answer(
                true,
                r#"[{"State":{"Running":true},"ImageName":"another"}]"#,
            ),
            ["rm", ..] => {
                self.container.store(false, Ordering::SeqCst);
                if self.rm_fixes.load(Ordering::SeqCst) {
                    self.broken.store(false, Ordering::SeqCst);
                }
                answer(true, "")
            }
            ["run", ..] => {
                self.container.store(true, Ordering::SeqCst);
                answer(true, "")
            }
            _ => answer(true, ""),
        })
    }
}

struct NoLock;

impl HostLock for NoLock {
    fn hold(&self) -> BrokerResult<Box<dyn std::any::Any>> {
        Ok(Box::new(()))
    }
}

/// Answers `ok` unless the container is `broken`.
struct FakeHealth(Arc<AtomicBool>);

impl HealthProbe for FakeHealth {
    fn probe(&self, _port: u16) -> Result<HealthResponse, String> {
        if self.0.load(Ordering::SeqCst) {
            Err("connection refused".to_owned())
        } else {
            Ok(HealthResponse::ok(VERSION))
        }
    }
}

struct FakeSource;

impl ImageSource for FakeSource {
    fn stage(&self, _dir: &Path) -> BrokerResult<String> {
        Ok("1.0".to_owned())
    }
}

fn broker_options(podman: &Arc<FakePodman>) -> BrokerOptions {
    BrokerOptions {
        ports: BrokerPorts {
            podman: podman.clone(),
            host_lock: Arc::new(NoLock),
            health: Arc::new(FakeHealth(podman.broken.clone())),
            source: Arc::new(FakeSource),
        },
        health_interval: Duration::from_millis(20),
        health_timeout: Duration::ZERO,
        client: None,
    }
}

/// The supervisor's options with the fake broker, and no host-wide
/// `host.toml` of the host's.
fn options_with(podman: &Arc<FakePodman>, dir: &Path, once: bool) -> SuperviseOptions {
    SuperviseOptions {
        broker: Some(broker_options(podman)),
        host_config: Some(dir.join("no-host.toml")),
        ..supervise_options(1, once)
    }
}

/// Commit `dagq.toml` with `[broker] mode = "<mode>"`.
fn broker_mode(repo: &Path, mode: &str) {
    fs::write(
        repo.join("dagq.toml"),
        format!("[broker]\nmode = \"{mode}\"\n"),
    )
    .unwrap();
    git(repo, &["add", "dagq.toml"]);
    git(repo, &["commit", "-m", "broker mode"]);
}

fn queue_dir(db: &Path) -> PathBuf {
    db.canonicalize().unwrap().parent().unwrap().to_path_buf()
}

fn kinds(db: &Path, like: &str) -> Vec<(String, Value)> {
    Connection::open(db)
        .unwrap()
        .prepare("SELECT kind, payload FROM run_events WHERE kind LIKE ?1 ORDER BY id")
        .unwrap()
        .query_map([like], |row| {
            let payload: String = row.get(1)?;
            Ok((row.get(0)?, serde_json::from_str(&payload).unwrap()))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

fn has_kind(db: &Path, kind: &str) -> bool {
    kinds(db, kind).iter().any(|(found, _)| found == kind)
}

fn broker_attention(db: &Path) -> Option<Value> {
    runtime::status(db).unwrap()["attention"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["kind"] == "broker_unhealthy")
        .cloned()
}

/// Without `[broker]`, and with `host.toml` lowering `preferred` to
/// `disabled`, the supervisor calls no podman and runs as before.
#[test]
fn a_disabled_broker_calls_no_podman() {
    let (_fixture, repo, db) = fixture();
    let podman = FakePodman::new(RUNNING);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let outcome =
        supervise_with(&db, &repo, &backend, &options_with(&podman, &repo, true)).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let run = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap()
        .runs[0]
        .clone();
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    assert!(podman.calls().is_empty(), "{:?}", podman.calls());
    // The worker started as before: no MCP configuration, no token, no
    // broker variable, no word of the tools in its prompt.
    let run_dir = PathBuf::from(run.run_dir().unwrap());
    assert!(!run_dir.join("broker").exists());
    assert!(!queue_dir(&db).join("broker").exists());
    assert!(
        !fs::read_to_string(run_dir.join("prompt.txt"))
            .unwrap()
            .contains("dagq-broker")
    );
    for tags in backend.tags.lock().unwrap().iter() {
        assert!(
            tags.env
                .iter()
                .all(|(name, _)| !name.starts_with("DAGQ_BROKER")),
            "{:?}",
            tags.env
        );
    }

    broker_mode(&repo, "preferred");
    fs::write(
        queue_dir(&db).join("host.toml"),
        "[broker]\nmode = \"disabled\"\n",
    )
    .unwrap();
    let outcome =
        supervise_with(&db, &repo, &backend, &options_with(&podman, &repo, true)).unwrap();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert!(podman.calls().is_empty(), "{:?}", podman.calls());
    assert!(kinds(&db, "broker_%").is_empty());
}

/// `required` is Phase 2's: the supervisor does not start rather than run
/// as `preferred`.
#[test]
fn a_required_broker_does_not_start_the_supervisor() {
    let (_fixture, repo, db) = fixture();
    broker_mode(&repo, "required");
    let podman = FakePodman::new(RUNNING);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let error = format!(
        "{:#}",
        supervise_with(&db, &repo, &backend, &options_with(&podman, &repo, true)).unwrap_err()
    );
    assert!(error.contains("mode = \"required\""), "{error}");
    assert!(podman.calls().is_empty());
}

/// On its first pass the supervisor makes the broker ready on a job
/// thread: while the image builds, `state.json` says `building` and the
/// task is claimed and run all the same. Once the build is done the
/// broker runs, recorded as `broker_image_built` and `broker_started`.
#[test]
fn the_image_builds_in_the_background_and_holds_no_claim() {
    let (_fixture, repo, db) = fixture();
    broker_mode(&repo, "preferred");
    let podman = FakePodman::new(RUNNING);
    podman.build_blocked.store(true, Ordering::SeqCst);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let outcome =
        supervise_with(&db, &repo, &backend, &options_with(&podman, &repo, true)).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let run = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap()
        .runs[0]
        .clone();
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    // Still building: the claim did not wait for it. (The job reaches the
    // build on its own thread; the build waits for this test.)
    let started = Instant::now();
    while BrokerState::read(&queue_dir(&db)).state.as_deref() != Some("building") {
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "never building"
        );
        thread::sleep(Duration::from_millis(20));
    }
    let state = BrokerState::read(&queue_dir(&db));
    assert_eq!(state.state.as_deref(), Some("building"), "{state:?}");
    assert_eq!(state.build.as_deref(), Some(VERSION));
    assert!(podman.called("machine list") >= 1);
    assert_eq!(podman.called(" build "), 1, "{:?}", podman.calls());
    assert!(!has_kind(&db, "broker_started"));

    // The build ends; the job goes on to the container and its health.
    podman.build_blocked.store(false, Ordering::SeqCst);
    let started = Instant::now();
    while BrokerState::read(&queue_dir(&db)).state.as_deref() != Some("running") {
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the broker never ran"
        );
        thread::sleep(Duration::from_millis(20));
    }
    // A supervisor started now finds the image and records the start.
    let outcome = supervise_with(&db, &repo, &backend, &{
        let stop = Arc::new(AtomicBool::new(false));
        let options = SuperviseOptions {
            stop: stop.clone(),
            ..options_with(&podman, &repo, false)
        };
        let db = db.clone();
        thread::spawn(move || {
            let started = Instant::now();
            while !has_kind(&db, "broker_started") && started.elapsed() < Duration::from_secs(30) {
                thread::sleep(Duration::from_millis(20));
            }
            stop.store(true, Ordering::SeqCst);
        });
        options
    })
    .unwrap();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let started = kinds(&db, "broker_started");
    assert_eq!(started.len(), 1, "{started:?}");
    assert_eq!(started[0].1["mode"], "preferred");
    assert_eq!(started[0].1["build"], VERSION);
    assert_eq!(podman.called(" build "), 1, "the image is built once");
}

/// Three failed looks in a row restart the container once
/// (`auto_repaired`); a broker that still fails is the inbox's attention
/// `broker_unhealthy` (next `dagq broker status`) until its health answers
/// again. Neither the drain nor anything else stops the container.
#[test]
fn three_failed_looks_restart_the_container_and_then_tell_the_inbox() {
    let (_fixture, repo, db) = fixture();
    SqliteQueue::open(&db)
        .unwrap()
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    broker_mode(&repo, "preferred");
    let podman = FakePodman::new(RUNNING);
    podman.image.store(true, Ordering::SeqCst);
    let backend = Arc::new(TestWorkspace::new(&db, false, VALID_AGENT));
    let stop = Arc::new(AtomicBool::new(false));
    let options = SuperviseOptions {
        stop: stop.clone(),
        ..options_with(&podman, &repo, false)
    };
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise_with(&db, &repo, &backend, &options))
    };
    wait_until(&db, Duration::from_secs(30), |_| {
        has_kind(&db, "broker_started")
    });
    assert_eq!(podman.called(" run --detach"), 1);

    // Broken; the restart mends it.
    podman.rm_fixes.store(true, Ordering::SeqCst);
    podman.broken.store(true, Ordering::SeqCst);
    wait_until(&db, Duration::from_secs(30), |_| {
        kinds(&db, "auto_repaired")
            .iter()
            .any(|(_, payload)| payload["repair"] == "broker_restart")
    });
    let repaired = kinds(&db, "auto_repaired");
    assert_eq!(repaired[0].1["conditions"]["failures"], 3, "{repaired:?}");
    assert_eq!(podman.called(" rm --force --time 10 "), 1);
    assert_eq!(podman.called(" run --detach"), 2);
    assert!(!has_kind(&db, "broker_unhealthy"));

    // Broken for good: after the restart the inbox is told, once.
    podman.rm_fixes.store(false, Ordering::SeqCst);
    podman.broken.store(true, Ordering::SeqCst);
    wait_until(&db, Duration::from_secs(30), |_| {
        has_kind(&db, "broker_unhealthy")
    });
    let attention = broker_attention(&db).expect("the attention");
    assert_eq!(attention["next"], "dagq broker status", "{attention}");
    assert_eq!(attention["status"], "unhealthy", "{attention}");
    thread::sleep(Duration::from_millis(200));
    assert_eq!(kinds(&db, "broker_unhealthy").len(), 1);

    // The broker answers again: the attention ends. A failed restart is
    // followed by starts from the machine up, so it ends with a start (a
    // look that answers would end it with broker_healthy).
    podman.broken.store(false, Ordering::SeqCst);
    wait_until(&db, Duration::from_secs(30), |_| {
        broker_attention(&db).is_none()
    });
    assert!(kinds(&db, "broker_started").len() >= 2 || has_kind(&db, "broker_healthy"));

    stop.store(true, Ordering::SeqCst);
    let outcome = joined(supervisor, "the supervisor to stop").unwrap();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    // The drain stopped neither the container nor the machine.
    assert_eq!(podman.called("machine stop"), 0);
    assert!(
        podman.called(" rm --force --time 10 ") >= 2,
        "{:?}",
        podman.calls()
    );
}

/// A person's machine running keeps dagq's from starting: the attention
/// `broker_unhealthy` with `reason: machine_busy`, and in `preferred` the
/// task is claimed and runs without the broker.
#[test]
fn a_busy_machine_tells_the_inbox_and_claims_go_on() {
    let (_fixture, repo, db) = fixture();
    broker_mode(&repo, "preferred");
    let podman = FakePodman::new(BUSY);
    let backend = Arc::new(TestWorkspace::new(&db, false, VALID_AGENT));
    let stop = Arc::new(AtomicBool::new(false));
    let options = SuperviseOptions {
        stop: stop.clone(),
        ..options_with(&podman, &repo, false)
    };
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise_with(&db, &repo, &backend, &options))
    };
    wait_until(&db, Duration::from_secs(30), |queue| {
        has_kind(&db, "broker_unhealthy")
            && queue
                .show(TaskId::new(1))
                .unwrap()
                .runs
                .first()
                .is_some_and(|run| run.status() == RunStatus::AwaitingIntegration)
    });
    stop.store(true, Ordering::SeqCst);
    let outcome = joined(supervisor, "the supervisor to stop").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let unhealthy = kinds(&db, "broker_unhealthy");
    assert_eq!(unhealthy[0].1["reason"], "machine_busy", "{unhealthy:?}");
    let attention = broker_attention(&db).expect("the attention");
    assert_eq!(attention["status"], "machine_busy", "{attention}");
    assert_eq!(attention["next"], "dagq broker status");
    // The person's machine was neither started nor stopped.
    assert_eq!(podman.called("machine start"), 0);
    assert_eq!(podman.called("machine stop"), 0);
    assert_eq!(
        BrokerState::read(&queue_dir(&db)).state.as_deref(),
        Some("machine_busy")
    );
    // The worker was claimed without the tools, and the run says why.
    let run = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap()
        .runs[0]
        .clone();
    let unavailable = events_of(&db, run.id(), "broker_unavailable");
    assert_eq!(unavailable.len(), 1, "{unavailable:?}");
    assert_eq!(unavailable[0]["reason"], "not_ready", "{unavailable:?}");
    assert!(events_of(&db, run.id(), "broker_token_issued").is_empty());
    assert!(
        !PathBuf::from(run.run_dir().unwrap())
            .join("broker")
            .exists()
    );
}

/// An exec (the handoff of `install` and the automatic update) leaves the
/// container running.
#[test]
fn a_handoff_leaves_the_broker_running() {
    let (_fixture, repo, db) = fixture();
    SqliteQueue::open(&db)
        .unwrap()
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    broker_mode(&repo, "preferred");
    let podman = FakePodman::new(RUNNING);
    podman.image.store(true, Ordering::SeqCst);
    let backend = Arc::new(TestWorkspace::new(&db, false, VALID_AGENT));
    let options = options_with(&podman, &repo, false);
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise_with(&db, &repo, &backend, &options))
    };
    wait_until(&db, Duration::from_secs(30), |_| {
        has_kind(&db, "broker_started")
    });
    let queue = SqliteQueue::open(&db).unwrap();
    let token = queue.supervisors().unwrap()[0].token.clone();
    assert!(queue.request_handoff(&token, "/next/dagq").unwrap());
    let outcome = joined(supervisor, "the supervisor asked to hand off").unwrap();
    assert_eq!(outcome["outcome"], "handoff", "{outcome}");
    assert_eq!(podman.called(" rm "), 0, "{:?}", podman.calls());
    assert_eq!(podman.called("machine stop"), 0);
}

/// `down` stops the broker after the drain as `dagq broker stop` does:
/// the container, then dagq's machine when nothing runs on it, recorded as
/// `broker_stopped`. With `disabled` it calls no podman.
#[test]
fn down_stops_the_broker_after_the_drain() {
    let (_fixture, repo, db) = fixture();
    // A supervisor binds the queue to the repository.
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let bind = FakePodman::new(RUNNING);
    supervise_with(&db, &repo, &backend, &options_with(&bind, &repo, true)).unwrap();
    backend.join();
    let location = QueueLocation::explicit(&db);
    let down = |podman: &Arc<FakePodman>| {
        let one_shot = OneShot {
            broker: Some(broker_options(podman)),
            host_config: Some(repo.join("no-host.toml")),
            ..OneShot::system()
        };
        one_shot
            .down(
                &location,
                &FakeCmux::default(),
                &FakeLaunchd::new(&db),
                &FakeProcesses::default(),
                &DownOptions {
                    wait: true,
                    force: false,
                    poll: Duration::from_millis(20),
                },
            )
            .unwrap()
    };
    let disabled = FakePodman::new(RUNNING);
    let report = down(&disabled);
    assert_eq!(report["outcome"], "not_running", "{report}");
    assert_eq!(report.get("broker"), None, "{report}");
    assert!(disabled.calls().is_empty());

    broker_mode(&repo, "preferred");
    let podman = FakePodman::new(RUNNING);
    podman.container.store(true, Ordering::SeqCst);
    let report = down(&podman);
    assert_eq!(report["broker"]["stopped"], true, "{report}");
    assert_eq!(
        report["broker"]["stop"]["machine_stopped"], true,
        "{report}"
    );
    let container = format!(
        "rm --force --time 10 dagq-broker-{}",
        QueueLocation::explicit(&db.canonicalize().unwrap()).hash()
    );
    assert_eq!(podman.called(&container), 1, "{:?}", podman.calls());
    assert_eq!(podman.called("machine stop dagq"), 1);
    let stopped = kinds(&db, "broker_stopped");
    assert_eq!(stopped.len(), 1);
    assert_eq!(stopped[0].1["container_stopped"], true);
    assert_eq!(
        BrokerState::read(&queue_dir(&db)).state.as_deref(),
        Some("stopped")
    );
}

/// The default `down` returns while the supervisor drains: it asks the
/// supervisors it signals (`broker_stop_requested`), and the supervisor
/// stops the broker once its drain ends, the container and then dagq's
/// machine, recorded as `broker_stopped` by the supervisor. A drain `down`
/// did not ask for (`up`'s replacement) leaves the broker running: see
/// `three_failed_looks_restart_the_container_and_then_tell_the_inbox`,
/// which stops its supervisor without `down`.
#[test]
fn the_default_down_has_the_supervisor_stop_the_broker_after_its_drain() {
    let (_fixture, repo, db) = fixture();
    SqliteQueue::open(&db)
        .unwrap()
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    broker_mode(&repo, "preferred");
    let podman = FakePodman::new(RUNNING);
    podman.image.store(true, Ordering::SeqCst);
    let backend = Arc::new(TestWorkspace::new(&db, false, VALID_AGENT));
    let stop = Arc::new(AtomicBool::new(false));
    let options = SuperviseOptions {
        stop: stop.clone(),
        ..options_with(&podman, &repo, false)
    };
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise_with(&db, &repo, &backend, &options))
    };
    wait_until(&db, Duration::from_secs(30), |_| {
        has_kind(&db, "broker_started")
    });
    let token = SqliteQueue::open(&db).unwrap().supervisors().unwrap()[0]
        .token
        .clone();

    // `down` without --wait: it asks and signals (the fake processes
    // deliver nothing), and stops nothing itself.
    let one_shot = OneShot {
        broker: Some(broker_options(&podman)),
        host_config: Some(repo.join("no-host.toml")),
        ..OneShot::system()
    };
    let processes = FakeProcesses::default();
    let report = one_shot
        .down(
            &QueueLocation::explicit(&db),
            &FakeCmux::default(),
            &FakeLaunchd::new(&db),
            &processes,
            &DownOptions {
                wait: false,
                force: false,
                poll: Duration::from_millis(20),
            },
        )
        .unwrap();
    assert_eq!(report["outcome"], "draining", "{report}");
    assert_eq!(report["broker"]["stopped"], false, "{report}");
    assert_eq!(processes.terminated.lock().unwrap().len(), 1);
    let requested = kinds(&db, "broker_stop_requested");
    assert_eq!(requested.len(), 1, "{requested:?}");
    assert_eq!(requested[0].1["supervisors"], json!([token]));
    assert_eq!(podman.called(" rm "), 0, "{:?}", podman.calls());
    assert_eq!(podman.called("machine stop"), 0);

    // The signal `down` sent: the supervisor drains, then stops the broker.
    stop.store(true, Ordering::SeqCst);
    let outcome = joined(supervisor, "the supervisor to drain").unwrap();
    assert_eq!(outcome["outcome"], "stopped", "{outcome}");
    let container = format!(
        "rm --force --time 10 dagq-broker-{}",
        QueueLocation::explicit(&db.canonicalize().unwrap()).hash()
    );
    assert_eq!(podman.called(&container), 1, "{:?}", podman.calls());
    assert_eq!(podman.called("machine stop dagq"), 1);
    let stopped = kinds(&db, "broker_stopped");
    assert_eq!(stopped.len(), 1, "{stopped:?}");
    assert_eq!(stopped[0].1["by"], "supervisor");
    assert_eq!(stopped[0].1["machine_stopped"], true);
    assert_eq!(
        BrokerState::read(&queue_dir(&db)).state.as_deref(),
        Some("stopped")
    );
}

/// Every file under `dir` whose bytes hold `needle`, but those under
/// `except`.
fn files_holding(dir: &Path, needle: &str, except: &[PathBuf]) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if except.iter().any(|skip| path.starts_with(skip)) {
                continue;
            }
            let kind = fs::symlink_metadata(&path).unwrap().file_type();
            if kind.is_dir() {
                stack.push(path);
            } else if kind.is_file()
                && fs::read(&path)
                    .unwrap()
                    .windows(needle.len())
                    .any(|window| window == needle.as_bytes())
            {
                found.push(path);
            }
        }
    }
    found
}

/// The queue's key and active marks served by the broker in process (no
/// podman), over the runs' dir.
fn serve_broker(queue: &Path, runs: &Path, audit: &Path) -> String {
    use dagq_broker::{
        backend::Backends,
        backends::{fs::FsBackend, git::GitBackend, process::ProcessBackend},
        config::Config,
        server::Server,
    };
    let text = |path: &Path| path.to_string_lossy().into_owned();
    let args = [
        "--listen".to_owned(),
        "127.0.0.1:0".to_owned(),
        "--key".to_owned(),
        text(&queue.join("broker/key")),
        "--active".to_owned(),
        text(&queue.join("broker/active")),
        "--audit".to_owned(),
        text(audit),
        "--root".to_owned(),
        text(&runs.canonicalize().unwrap()),
    ];
    let config = Config::parse(&args).unwrap();
    let backends = Backends {
        fs: Arc::new(FsBackend::new(config.roots.clone())),
        process: Arc::new(ProcessBackend::new(config.roots.clone())),
        git: Arc::new(GitBackend::new(config.roots.clone())),
    };
    let server = Server::bind(&config, backends).unwrap();
    let addr = server.local_addr().unwrap();
    thread::spawn(move || server.serve());
    format!("http://{addr}")
}

fn list_workspace(url: &str, token_file: &Path) -> Result<Vec<String>, String> {
    use dagq_broker_client::{BrokerClient, Endpoint};
    let client = BrokerClient::new(Endpoint::parse(url).unwrap(), Some(token_file.to_owned()))
        .with_read_timeout(Duration::from_secs(30));
    client
        .fs_list(&dagq_broker_protocol::fs::ListRequest { path: ".".into() })
        .map(|listed| listed.entries.into_iter().map(|entry| entry.name).collect())
        .map_err(|error| match error.broker_error() {
            Some(refused) => refused.code.as_str().to_owned(),
            None => format!("{error}"),
        })
}

/// With `preferred` and a broker that runs, the claim issues the run's
/// token: its file in `<queue dir>/broker/tokens` (mode 0600), the MCP
/// configuration naming that file in the run's dir, the tools in the
/// worker's prompt, `broker_token_issued` with the `jti`. The token opens
/// the worker's workspace on the broker, and its value is nowhere else:
/// not in the prompt, an event, a log, the configuration or the
/// workspace's environment. When the run ends (integrated here), the next
/// pass revokes it (`broker_token_revoked`, `reason: integrated`): its
/// file, its mark and the configuration go, and the broker refuses it.
#[test]
fn a_preferred_worker_gets_its_token_and_the_end_of_its_run_revokes_it() {
    use std::os::unix::fs::PermissionsExt;
    let (fixture, repo, db) = fixture();
    SqliteQueue::open(&db)
        .unwrap()
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    broker_mode(&repo, "preferred");
    let podman = FakePodman::new(RUNNING);
    podman.image.store(true, Ordering::SeqCst);
    let client = fixture.dir.path().join("dagq-broker-client");
    fs::write(&client, "").unwrap();
    let backend = Arc::new(TestWorkspace::new(&db, false, VALID_AGENT));
    let stop = Arc::new(AtomicBool::new(false));
    let options = SuperviseOptions {
        stop: stop.clone(),
        broker: Some(BrokerOptions {
            client: Some(client.clone()),
            ..broker_options(&podman)
        }),
        ..options_with(&podman, &repo, false)
    };
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise_with(&db, &repo, &backend, &options))
    };
    wait_until(&db, Duration::from_secs(30), |_| {
        has_kind(&db, "broker_started")
    });
    let task = add_ready_task(&mut SqliteQueue::open(&db).unwrap(), "brokered", &[]);
    wait_until(&db, Duration::from_secs(60), |queue| {
        queue
            .show(task)
            .unwrap()
            .runs
            .first()
            .is_some_and(|run| run.status() == RunStatus::AwaitingIntegration)
    });
    stop.store(true, Ordering::SeqCst);
    let outcome = joined(supervisor, "the supervisor to stop").unwrap();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");

    let run = SqliteQueue::open(&db).unwrap().show(task).unwrap().runs[0].clone();
    let queue = queue_dir(&db);
    let run_dir = PathBuf::from(run.run_dir().unwrap());
    let issued = events_of(&db, run.id(), "broker_token_issued");
    assert_eq!(issued.len(), 1, "{issued:?}");
    let jti = issued[0]["jti"].as_str().unwrap().to_owned();
    assert_eq!(
        issued[0]["capabilities"],
        json!([
            "fs.read",
            "fs.write",
            "process.exec",
            "git.read",
            "git.write"
        ])
    );
    assert!(issued[0]["exp"].as_u64().unwrap() > 0);
    let token_file = queue.join("broker/tokens").join(run.id().as_str());
    assert_eq!(
        fs::metadata(&token_file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let token = fs::read_to_string(&token_file).unwrap().trim().to_owned();
    assert!(
        token.starts_with("dagq1."),
        "the token file holds the token"
    );
    assert_eq!(
        fs::read_to_string(queue.join("broker/active").join(&jti)).unwrap(),
        run.id().as_str()
    );
    let config: Value =
        serde_json::from_str(&fs::read_to_string(run_dir.join("broker/mcp.json")).unwrap())
            .unwrap();
    let server = &config["mcpServers"]["dagq-broker"];
    assert_eq!(server["command"], json!(client));
    assert_eq!(server["args"], json!(["mcp"]));
    assert_eq!(server["env"]["DAGQ_BROKER_TOKEN_FILE"], json!(token_file));
    assert!(
        server["env"]["DAGQ_BROKER_URL"]
            .as_str()
            .unwrap()
            .starts_with("http://127.0.0.1:")
    );
    assert!(
        fs::read_to_string(run_dir.join("prompt.txt"))
            .unwrap()
            .contains("mcp__dagq-broker__read_file")
    );
    // The value is in its file only (the key signs it): not in the queue
    // DB, a prompt, a log, the configuration or the workspace's env.
    let leaks = files_holding(
        &fixture.dir.path().canonicalize().unwrap(),
        &token,
        &[queue.join("broker/tokens"), queue.join("broker/key")],
    );
    assert!(leaks.is_empty(), "the token leaked into {leaks:?}");
    for tags in backend.tags.lock().unwrap().iter() {
        assert!(tags.env.iter().all(|(_, value)| !value.contains(&token)));
    }

    // `status` and `doctor` show the mode, the health the supervisor last
    // recorded and the token held; the actors stay advisory hosts.
    for report in [
        runtime::status(&db).unwrap(),
        runtime::doctor(&db, false).unwrap(),
    ] {
        let broker = &report["broker"];
        assert_eq!(broker["mode"], "preferred", "{broker}");
        assert_eq!(broker["health"]["state"], "healthy", "{broker}");
        assert_eq!(broker["active_tokens"], 1, "{broker}");
    }

    // The broker takes the token for the run's workspace.
    let audit = tempfile::tempdir().unwrap();
    let url = serve_broker(&queue, run_dir.parent().unwrap(), audit.path());
    let kept = fixture.dir.path().join("kept-token");
    fs::write(&kept, format!("{token}\n")).unwrap();
    let listed = list_workspace(&url, &kept).unwrap();
    assert!(listed.contains(&"seed.txt".to_owned()), "{listed:?}");

    // A token with less than 4 hours left is issued again: a new jti, the
    // older one retired. The token file is rewritten to expire soon.
    let key =
        dagq_broker_protocol::SigningKey::from_bytes(&fs::read(queue.join("broker/key")).unwrap())
            .unwrap();
    let mut claims = dagq_broker_protocol::BrokerSessionToken::new(token.clone())
        .unverified_claims()
        .unwrap();
    claims.exp = claims.iat + 60;
    let soon = dagq_broker_protocol::sign(&key, &claims).unwrap();
    fs::write(&token_file, format!("{}\n", soon.expose())).unwrap();
    // A new supervisor's first pass issues it: before its own start of the
    // broker came back, it uses the broker recorded as running dagq's
    // build that answers now.
    let options = SuperviseOptions {
        broker: Some(BrokerOptions {
            client: Some(client.clone()),
            ..broker_options(&podman)
        }),
        ..options_with(&podman, &repo, true)
    };
    let outcome = supervise_with(&db, &repo, &backend, &options).unwrap();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let issued = events_of(&db, run.id(), "broker_token_issued");
    assert_eq!(issued.len(), 2, "{issued:?}");
    assert_eq!(issued[1]["renews"], json!(jti));
    let jti = issued[1]["jti"].as_str().unwrap().to_owned();
    assert_eq!(
        events_of(&db, run.id(), "broker_token_revoked"),
        [json!({"jti": claims.jti, "reason": "renewed"})]
    );
    assert!(!queue.join("broker/active").join(&claims.jti).exists());
    assert!(queue.join("broker/active").join(&jti).exists());
    assert_eq!(list_workspace(&url, &kept), Err("unauthorized".to_owned()));
    let token = fs::read_to_string(&token_file).unwrap().trim().to_owned();
    fs::write(&kept, format!("{token}\n")).unwrap();
    assert!(list_workspace(&url, &kept).is_ok());

    // The run ends; the next pass revokes the token.
    integrate(&db, task.as_i64(), &repo).unwrap();
    assert_eq!(
        SqliteQueue::open(&db)
            .unwrap()
            .run(run.id())
            .unwrap()
            .status(),
        RunStatus::Integrated
    );
    let outcome =
        supervise_with(&db, &repo, &backend, &options_with(&podman, &repo, true)).unwrap();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let revoked = events_of(&db, run.id(), "broker_token_revoked");
    assert_eq!(revoked[1], json!({"jti": jti, "reason": "integrated"}));
    assert!(!token_file.exists());
    assert!(!queue.join("broker/active").join(&jti).exists());
    assert!(!run_dir.join("broker").exists());
    assert_eq!(list_workspace(&url, &kept), Err("unauthorized".to_owned()));
    assert_eq!(runtime::status(&db).unwrap()["broker"]["active_tokens"], 0);
}

/// Whatever way a run ends, its token is revoked with the run's status as
/// the reason: `failed` (the session died without a receipt; the running
/// supervisor's next pass), and `interrupted` and `succeeded` (the other
/// ended statuses, set here as `recover` and an older landing leave them;
/// the next supervisor's first pass). The token file, the mark and the
/// run's MCP configuration go each time, and no token is left held.
#[test]
fn a_run_that_fails_or_is_interrupted_loses_its_token() {
    let (fixture, repo, db) = fixture();
    SqliteQueue::open(&db)
        .unwrap()
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    broker_mode(&repo, "preferred");
    let podman = FakePodman::new(RUNNING);
    podman.image.store(true, Ordering::SeqCst);
    let client = fixture.dir.path().join("dagq-broker-client");
    fs::write(&client, "").unwrap();
    let backend = Arc::new(TestWorkspace::new(&db, false, VALID_AGENT));
    let broker = || {
        Some(BrokerOptions {
            client: Some(client.clone()),
            ..broker_options(&podman)
        })
    };
    let stop = Arc::new(AtomicBool::new(false));
    let options = SuperviseOptions {
        stop: stop.clone(),
        broker: broker(),
        ..options_with(&podman, &repo, false)
    };
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise_with(&db, &repo, &backend, &options))
    };
    wait_until(&db, Duration::from_secs(30), |_| {
        has_kind(&db, "broker_started")
    });
    let (failing, interrupted, succeeded) = {
        let mut queue = SqliteQueue::open(&db).unwrap();
        let failing = add_ready_task(&mut queue, "dies", &[]);
        backend
            .scripts
            .lock()
            .unwrap()
            .insert(failing, "exit 7".to_owned());
        (
            failing,
            add_ready_task(&mut queue, "interrupted later", &[]),
            add_ready_task(&mut queue, "succeeded later", &[]),
        )
    };
    let run_of = |task: TaskId| SqliteQueue::open(&db).unwrap().show(task).unwrap().runs[0].clone();
    wait_until(&db, Duration::from_secs(60), |queue| {
        let failed = queue
            .show(failing)
            .unwrap()
            .runs
            .first()
            .is_some_and(|run| !events_of(&db, run.id(), "broker_token_revoked").is_empty());
        failed
            && [interrupted, succeeded].iter().all(|task| {
                queue
                    .show(*task)
                    .unwrap()
                    .runs
                    .first()
                    .is_some_and(|run| run.status() == RunStatus::AwaitingIntegration)
            })
    });
    stop.store(true, Ordering::SeqCst);
    let outcome = joined(supervisor, "the supervisor to stop").unwrap();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");

    let queue = queue_dir(&db);
    let assert_revoked = |task: TaskId, reason: &str| {
        let run = run_of(task);
        let issued = events_of(&db, run.id(), "broker_token_issued");
        assert_eq!(issued.len(), 1, "{task} {issued:?}");
        let jti = issued[0]["jti"].as_str().unwrap();
        assert_eq!(
            events_of(&db, run.id(), "broker_token_revoked"),
            [json!({"jti": jti, "reason": reason})],
            "{task}"
        );
        assert!(!queue.join("broker/active").join(jti).exists(), "{task}");
        assert!(
            !queue.join("broker/tokens").join(run.id().as_str()).exists(),
            "{task}"
        );
        assert!(
            !PathBuf::from(run.run_dir().unwrap())
                .join("broker")
                .exists(),
            "{task}"
        );
    };
    let failed = run_of(failing);
    assert_eq!(failed.status(), RunStatus::Failed);
    assert_revoked(failing, "failed");
    // The two others still hold theirs until they end.
    assert_eq!(runtime::status(&db).unwrap()["broker"]["active_tokens"], 2);
    let raw = Connection::open(&db).unwrap();
    for (task, status) in [(interrupted, "interrupted"), (succeeded, "succeeded")] {
        raw.execute(
            "UPDATE task_runs SET status=?2 WHERE id=?1",
            rusqlite::params![run_of(task).id().as_str(), status],
        )
        .unwrap();
    }
    let options = SuperviseOptions {
        broker: broker(),
        ..options_with(&podman, &repo, true)
    };
    let outcome = supervise_with(&db, &repo, &backend, &options).unwrap();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_revoked(interrupted, "interrupted");
    assert_revoked(succeeded, "succeeded");
    assert_eq!(runtime::status(&db).unwrap()["broker"]["active_tokens"], 0);
}

/// A queue run `preferred` and then put back to `disabled` (task 1125):
/// the disabled supervisor's first pass revokes every token left, the
/// live run's (awaiting its landing) and the ended one's (interrupted
/// while no supervisor ran) alike, with the reason `mode_disabled`: the
/// marks, the token files and the runs' MCP configurations go. It calls
/// no podman for it.
#[test]
fn a_disabled_supervisor_revokes_the_tokens_an_earlier_mode_left() {
    let (fixture, repo, db) = fixture();
    SqliteQueue::open(&db)
        .unwrap()
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    broker_mode(&repo, "preferred");
    let podman = FakePodman::new(RUNNING);
    podman.image.store(true, Ordering::SeqCst);
    let client = fixture.dir.path().join("dagq-broker-client");
    fs::write(&client, "").unwrap();
    let backend = Arc::new(TestWorkspace::new(&db, false, VALID_AGENT));
    let stop = Arc::new(AtomicBool::new(false));
    let options = SuperviseOptions {
        stop: stop.clone(),
        broker: Some(BrokerOptions {
            client: Some(client.clone()),
            ..broker_options(&podman)
        }),
        ..options_with(&podman, &repo, false)
    };
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise_with(&db, &repo, &backend, &options))
    };
    wait_until(&db, Duration::from_secs(30), |_| {
        has_kind(&db, "broker_started")
    });
    let (live, ended) = {
        let mut queue = SqliteQueue::open(&db).unwrap();
        (
            add_ready_task(&mut queue, "lands later", &[]),
            add_ready_task(&mut queue, "interrupted later", &[]),
        )
    };
    let run_of = |task: TaskId| SqliteQueue::open(&db).unwrap().show(task).unwrap().runs[0].clone();
    wait_until(&db, Duration::from_secs(60), |queue| {
        [live, ended].iter().all(|task| {
            queue
                .show(*task)
                .unwrap()
                .runs
                .first()
                .is_some_and(|run| run.status() == RunStatus::AwaitingIntegration)
        })
    });
    stop.store(true, Ordering::SeqCst);
    let outcome = joined(supervisor, "the supervisor to stop").unwrap();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE task_runs SET status='interrupted' WHERE id=?1",
            [run_of(ended).id().as_str()],
        )
        .unwrap();
    let queue = queue_dir(&db);
    let jtis: Vec<(TaskId, String)> = [live, ended]
        .into_iter()
        .map(|task| {
            let run = run_of(task);
            let issued = events_of(&db, run.id(), "broker_token_issued");
            assert_eq!(issued.len(), 1, "{task} {issued:?}");
            let run_dir = PathBuf::from(run.run_dir().unwrap());
            assert!(run_dir.join("broker/mcp.json").is_file(), "{task}");
            (task, issued[0]["jti"].as_str().unwrap().to_owned())
        })
        .collect();
    assert_eq!(runtime::status(&db).unwrap()["broker"]["active_tokens"], 2);

    // Back to `disabled` (host.toml lowers it): the first pass revokes both.
    fs::write(queue.join("host.toml"), "[broker]\nmode = \"disabled\"\n").unwrap();
    let calls = podman.calls().len();
    let outcome =
        supervise_with(&db, &repo, &backend, &options_with(&podman, &repo, true)).unwrap();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(podman.calls().len(), calls, "{:?}", podman.calls());
    for (task, jti) in &jtis {
        let run = run_of(*task);
        assert_eq!(
            events_of(&db, run.id(), "broker_token_revoked"),
            [json!({"jti": jti, "reason": "mode_disabled"})],
            "{task}"
        );
        assert!(!queue.join("broker/active").join(jti).exists(), "{task}");
        assert!(
            !queue.join("broker/tokens").join(run.id().as_str()).exists(),
            "{task}"
        );
        assert!(
            !PathBuf::from(run.run_dir().unwrap())
                .join("broker")
                .exists(),
            "{task}"
        );
        assert_eq!(
            dagq::application::broker_run::worker_mcp_config(&run),
            None,
            "{task}"
        );
    }
    assert_eq!(runtime::status(&db).unwrap()["broker"]["active_tokens"], 0);
    // Nothing left to revoke: the next pass records nothing more.
    let outcome =
        supervise_with(&db, &repo, &backend, &options_with(&podman, &repo, true)).unwrap();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(kinds(&db, "broker_token_revoked").len(), 2);
    assert_eq!(podman.calls().len(), calls);
}

/// A run parked as `needs_session` that an earlier `preferred` left its
/// token file and MCP configuration, but no active mark the sweep would
/// find (a revoke that failed partway), is resumed by a `disabled`
/// supervisor without the broker's tools (task 1125): the resume's grant
/// removes what is left, so the configuration (what the executor hands the
/// resume and a headless turn as `--mcp-config` and `--allowedTools
/// mcp__dagq-broker`) is gone before the resumed session starts, its
/// resume message says nothing of the tools, and no token is issued
/// again.
#[test]
fn a_disabled_supervisor_resumes_a_run_without_the_tools_left_to_it() {
    use dagq::application::broker_run::{Grant, RunTokens, worker_mcp_config};
    let (fixture, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (run, _) = parked_conflict(&repo, &db, &backend);
    let queue = queue_dir(&db);
    let tokens = dagq::infrastructure::broker_token::QueueRunTokens {
        queue_dir: queue.clone(),
    };
    let client = fixture.dir.path().join("dagq-broker-client");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let issued = tokens
        .issue(&run, &Grant { client, port: 8750 }, now)
        .unwrap();
    tokens.retire(&issued.jti).unwrap();
    assert!(tokens.held().unwrap().is_empty());
    let run_dir = PathBuf::from(run.run_dir().unwrap());
    assert!(worker_mcp_config(&run).is_some());

    backend.resume_script_for(
        2,
        "await_message; dir=\"$(dirname \"$RECEIPT\")\"; [ -e \"$dir/broker\" ] && : > \"$dir/broker-seen\"; grep -q dagq-broker \"$MESSAGE\" && : > \"$dir/broker-told\"; resolve; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    let podman = FakePodman::new(RUNNING);
    let outcome =
        supervise_with(&db, &repo, &backend, &options_with(&podman, &repo, true)).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert!(podman.calls().is_empty(), "{:?}", podman.calls());
    assert!(
        !events_of(&db, run.id(), "resume_started").is_empty(),
        "the run was resumed"
    );
    assert!(!run_dir.join("broker-seen").exists());
    assert!(!run_dir.join("broker-told").exists());
    assert!(!run_dir.join("broker").exists());
    // No mark was left, so no token to record as revoked or issued.
    assert_eq!(kinds(&db, "broker_%"), []);
    assert!(!queue.join("broker/tokens").join(run.id().as_str()).exists());
    assert!(tokens.held().unwrap().is_empty());
}
